//! Block device discovery: prefer `lsblk -J`, otherwise fall back to sysfs + blkid.
//!
//! `lsblk -J` prints JSON. No JSON dependency is pulled in, so this module carries
//! a minimal JSON parser that only covers the syntax lsblk output needs: object /
//! array / string / number / true / false / null; strings support
//! `\" \\ \/ \b \f \n \r \t` and `\uXXXX` (including UTF-16 surrogate pairs).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::exec::CommandRunner;
use crate::logging;
use crate::model::{BlockDevice, MountEntry};

/// Full column set (newer util-linux).
pub const FULL_COLUMNS: &str =
    "NAME,TYPE,FSTYPE,LABEL,UUID,PARTUUID,SIZE,RM,RO,HOTPLUG,TRAN,MOUNTPOINT,MODEL,SERIAL,PKNAME";
/// Minimal column set for older util-linux releases.
pub const MINIMAL_COLUMNS: &str = "NAME,TYPE,FSTYPE,LABEL,UUID,SIZE,RM,RO,MOUNTPOINT";

/// Block device directory in sysfs.
const SYSFS_BLOCK_DIR: &str = "/sys/class/block";
/// Probe timeout so an unresponsive device cannot stall the daemon.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// List all current block devices.
pub fn list_devices(
    runner: &dyn CommandRunner,
    include_loop: bool,
    prefer_lsblk: bool,
) -> Vec<BlockDevice> {
    let mut devices: Vec<BlockDevice> = Vec::new();

    if prefer_lsblk && runner.which("lsblk") {
        devices = lsblk_devices(runner, FULL_COLUMNS)
            .or_else(|| lsblk_devices(runner, MINIMAL_COLUMNS))
            .unwrap_or_default();
    }

    if devices.is_empty() && Path::new(SYSFS_BLOCK_DIR).is_dir() {
        logging::debug(format!(
            "lsblk unavailable or its output could not be parsed; enumerating block devices from {SYSFS_BLOCK_DIR}"
        ));
        devices = list_sysfs(Path::new(SYSFS_BLOCK_DIR), runner);
    }

    if !include_loop {
        devices.retain(|device| !is_loop_device(device));
    }
    devices
}

/// Parse `lsblk -J` output (nested JSON is flattened).
pub fn parse_lsblk(text: &str) -> Result<Vec<BlockDevice>> {
    let text = text.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return Err(
            Error::device("lsblk produced no output, cannot parse the device list").with_hint(
                "Make sure util-linux is installed correctly (lsblk -J should print JSON)",
            ),
        );
    }
    let document = JsonParser::new(text).parse_document().map_err(|reason| {
        Error::device(format!("failed to parse lsblk JSON: {reason}"))
            .with_hint("Make sure lsblk comes from util-linux 2.27 or newer")
    })?;

    let Some(blockdevices) = document.get("blockdevices") else {
        return Err(Error::device("lsblk JSON has no blockdevices field")
            .with_hint("Make sure the command is 'lsblk -J -b -o ...'"));
    };
    let Some(items) = blockdevices.as_array() else {
        return Err(Error::device("blockdevices in lsblk JSON is not an array"));
    };

    let mut devices = Vec::new();
    for item in items {
        flatten_node(item, None, &mut devices);
    }
    link_children(&mut devices);
    Ok(devices)
}

/// Read filesystem information with blkid; returns an empty map on failure or when there is no filesystem.
pub fn probe_blkid(runner: &dyn CommandRunner, device_path: &str) -> BTreeMap<String, String> {
    if !runner.which("blkid") {
        logging::debug("blkid not found, skipping filesystem probing");
        return BTreeMap::new();
    }
    let output = match runner.run("blkid", &["-o", "export", device_path], Some(PROBE_TIMEOUT)) {
        Ok(output) => output,
        Err(err) => {
            logging::debug(format!("failed to probe {device_path}: {err}"));
            return BTreeMap::new();
        }
    };
    if !output.ok() {
        // blkid exits non-zero for devices without a filesystem, which is expected.
        logging::debug(format!(
            "blkid found no filesystem on {device_path} (exit code {})",
            output.status
        ));
        return BTreeMap::new();
    }
    parse_blkid_export(&output.stdout)
}

/// Re-probe the filesystem (a udev event may arrive before the partition is ready).
pub fn refresh_device(device: &BlockDevice, runner: &dyn CommandRunner) -> BlockDevice {
    let mut updated = device.clone();
    let missing_fstype = is_blank(updated.fstype.as_deref());
    let missing_uuid = is_blank(updated.uuid.as_deref());
    if !missing_fstype && !missing_uuid {
        return updated;
    }

    let info = probe_blkid(runner, &updated.path);
    if info.is_empty() {
        logging::debug(format!(
            "{} still has no discoverable filesystem info",
            updated.path
        ));
        return updated;
    }
    if missing_fstype {
        if let Some(fstype) = info.get("TYPE").filter(|value| !value.trim().is_empty()) {
            updated.fstype = Some(fstype.trim().to_string());
        }
    }
    if missing_uuid {
        if let Some(uuid) = info.get("UUID").filter(|value| !value.trim().is_empty()) {
            updated.uuid = Some(uuid.trim().to_string());
        }
    }
    if is_blank(updated.label.as_deref()) {
        if let Some(label) = info.get("LABEL").filter(|value| !value.trim().is_empty()) {
            updated.label = Some(label.trim().to_string());
        }
    }
    updated
}

/// Find a device by name / path / UUID / PARTUUID / label.
///
/// Accepts forms such as `sdb1`, `/dev/sdb1`, `UUID=xxxx`, `/dev/disk/by-uuid/xxxx`,
/// `LABEL=backup-disk` and `backup-disk`.
pub fn resolve_device(devices: &[BlockDevice], reference: &str) -> Result<BlockDevice> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(
            Error::device("device reference must not be empty").with_hint(
                "Use UUID=, /dev/sdb1, /dev/disk/by-uuid/... or a volume label to name a device",
            ),
        );
    }

    let matches: Vec<&BlockDevice> =
        if let Some(rest) = strip_prefix_ci(reference, "/dev/disk/by-uuid/") {
            by_uuid(devices, rest)
        } else if let Some(rest) = strip_prefix_ci(reference, "/dev/disk/by-partuuid/") {
            by_partuuid(devices, rest)
        } else if let Some(rest) = strip_prefix_ci(reference, "/dev/disk/by-label/") {
            by_label(devices, &unescape_by_label(rest))
        } else if let Some(rest) = strip_prefix_ci(reference, "uuid=") {
            by_uuid(devices, rest)
        } else if let Some(rest) = strip_prefix_ci(reference, "uuid:") {
            by_uuid(devices, rest)
        } else if let Some(rest) = strip_prefix_ci(reference, "partuuid=") {
            by_partuuid(devices, rest)
        } else if let Some(rest) = strip_prefix_ci(reference, "partuuid:") {
            by_partuuid(devices, rest)
        } else if let Some(rest) = strip_prefix_ci(reference, "label=") {
            by_label(devices, rest)
        } else {
            by_name_or_label(devices, reference)
        };

    match matches.as_slice() {
        [] => Err(Error::device(format!("device not found: {reference}")).with_hint(
            "Run 'automountd list --all' to see the available block devices; you can also specify one with UUID= or a full device path",
        )),
        [only] => Ok((*only).clone()),
        many => {
            let candidates: Vec<String> = many
                .iter()
                .map(|device| {
                    let label = device.label.as_deref().unwrap_or("-");
                    format!("{} ({}, label {label})", device.path, device.dev_type)
                })
                .collect();
            Err(Error::device(format!(
                "device reference {reference} matches {} devices: {}",
                many.len(),
                candidates.join(", ")
            ))
            .with_hint("Use UUID=... or a full device path to identify the device unambiguously"))
        }
    }
}

/// Look up by device name.
pub fn find_by_name(devices: &[BlockDevice], name: &str) -> Option<BlockDevice> {
    let wanted = name.trim();
    if wanted.is_empty() {
        return None;
    }
    let bare = strip_prefix_ci(wanted, "/dev/").unwrap_or(wanted);
    let devnode = format!("/dev/{bare}");
    devices
        .iter()
        .find(|device| {
            device.name == wanted
                || device.name == bare
                || device.path == wanted
                || device.path == devnode
        })
        .cloned()
}

/// Walk up to the whole-disk name (sda1 -> sda).
pub fn top_level_name(devices: &[BlockDevice], device: &BlockDevice) -> String {
    let mut current = device.name.clone();
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(current.clone());

    for _ in 0..64 {
        let Some(found) = devices.iter().find(|candidate| candidate.name == current) else {
            break;
        };
        let Some(parent) = found
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|parent| !parent.is_empty())
        else {
            break;
        };
        if !seen.insert(parent.to_string()) {
            logging::warn(format!(
                "parent/child relationship of device {} contains a cycle; stopped walking up",
                device.name
            ));
            break;
        }
        if devices.iter().any(|candidate| candidate.name == parent) {
            current = parent.to_string();
            continue;
        }
        // The parent is not in the list (only partition info is known, say), so the
        // parent name is itself the whole-disk name.
        return parent.to_string();
    }
    current
}

/// Find the whole-disk names backing critical system directories (/, /boot, /usr, ...).
pub fn system_disk_names(devices: &[BlockDevice], mounts: &[MountEntry]) -> HashSet<String> {
    /// Critical system mount points: the disks holding these directories are never auto-mounted.
    const CRITICAL_TARGETS: &[&str] = &["/", "/boot", "/boot/efi", "/usr", "/var", "/etc"];

    let mut names = HashSet::new();
    for entry in mounts {
        if !CRITICAL_TARGETS
            .iter()
            .any(|target| same_mount_path(&entry.target, target))
        {
            continue;
        }
        if !entry.source.starts_with("/dev/") {
            continue;
        }

        let mut candidates: Vec<String> = Vec::new();
        let direct = basename(&entry.source).to_string();
        if !direct.is_empty() {
            candidates.push(direct);
        }
        // Symlinks such as /dev/disk/by-uuid/... must be resolved to the real device node first.
        if let Ok(real) = std::fs::canonicalize(&entry.source) {
            let resolved = basename(&real.to_string_lossy()).to_string();
            if !resolved.is_empty() && !candidates.contains(&resolved) {
                candidates.push(resolved);
            }
        }

        let mut matched = false;
        for candidate in &candidates {
            if let Some(device) = find_by_name(devices, candidate) {
                names.insert(top_level_name(devices, &device));
                matched = true;
            }
        }
        if !matched {
            // The device is not in the current list (container, restricted permissions,
            // ...), so at least protect the node names we know about.
            logging::debug(format!(
                "device {} of system mount point {} is not in the device list; protecting it by node name",
                entry.target, entry.source
            ));
            for candidate in candidates {
                names.insert(candidate);
            }
        }
    }
    names
}

// ---------------------------------------------------------------------------
// lsblk JSON parsing
// ---------------------------------------------------------------------------

type JsonResult<T> = std::result::Result<T, String>;

/// Minimal JSON value.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(String),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(pairs) => pairs
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }
}

/// Hand-written JSON parser (parsing only, no serialization).
struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn parse_document(mut self) -> JsonResult<Json> {
        self.skip_ws();
        let value = self.parse_value(0)?;
        self.skip_ws();
        if self.pos < self.bytes.len() {
            return Err(format!(
                "trailing content after JSON document (offset {})",
                self.pos
            ));
        }
        Ok(value)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_ws(&mut self) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn expect(&mut self, expected: u8) -> JsonResult<()> {
        match self.bump() {
            Some(byte) if byte == expected => Ok(()),
            Some(byte) => Err(format!(
                "at offset {} expected '{}', found '{}'",
                self.pos - 1,
                expected as char,
                byte as char
            )),
            None => Err(format!(
                "unexpected end of JSON, expected '{}'",
                expected as char
            )),
        }
    }

    fn parse_value(&mut self, depth: usize) -> JsonResult<Json> {
        if depth > 96 {
            return Err("JSON nesting is too deep".to_string());
        }
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => Ok(Json::Str(self.parse_string()?)),
            Some(b't') => self.parse_literal("true", Json::Bool(true)),
            Some(b'f') => self.parse_literal("false", Json::Bool(false)),
            Some(b'n') => self.parse_literal("null", Json::Null),
            Some(byte) if byte == b'-' || byte.is_ascii_digit() => self.parse_number(),
            Some(byte) => Err(format!(
                "at offset {} found invalid character '{}'",
                self.pos, byte as char
            )),
            None => Err("JSON ended unexpectedly while reading a value".to_string()),
        }
    }

    fn parse_literal(&mut self, literal: &str, value: Json) -> JsonResult<Json> {
        let end = self.pos + literal.len();
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| format!("incomplete literal at offset {}", self.pos))?;
        if slice != literal.as_bytes() {
            return Err(format!("expected literal {literal} at offset {}", self.pos));
        }
        self.pos = end;
        Ok(value)
    }

    fn parse_number(&mut self) -> JsonResult<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(byte) = self.peek() {
            let acceptable = byte.is_ascii_digit()
                || byte == b'.'
                || byte == b'e'
                || byte == b'E'
                || byte == b'+'
                || byte == b'-';
            if !acceptable {
                break;
            }
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|err| format!("number at offset {start} is not valid UTF-8: {err}"))?;
        if text.is_empty() || text.parse::<f64>().is_err() {
            return Err(format!("invalid number at offset {start}: {text:?}"));
        }
        Ok(Json::Number(text.to_string()))
    }

    fn parse_array(&mut self, depth: usize) -> JsonResult<Json> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                Some(byte) => {
                    return Err(format!(
                        "at offset {} expected ',' or ']', found '{}'",
                        self.pos - 1,
                        byte as char
                    ))
                }
                None => return Err("JSON array is not closed".to_string()),
            }
        }
        Ok(Json::Array(items))
    }

    fn parse_object(&mut self, depth: usize) -> JsonResult<Json> {
        self.expect(b'{')?;
        let mut pairs: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(format!(
                    "object key at offset {} must be a string",
                    self.pos
                ));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.parse_value(depth + 1)?;
            pairs.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                Some(byte) => {
                    return Err(format!(
                        "at offset {} expected ',' or '}}', found '{}'",
                        self.pos - 1,
                        byte as char
                    ))
                }
                None => return Err("JSON object is not closed".to_string()),
            }
        }
        Ok(Json::Object(pairs))
    }

    fn parse_string(&mut self) -> JsonResult<String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.bump() else {
                return Err("JSON string is not closed".to_string());
            };
            match byte {
                b'"' => break,
                b'\\' => {
                    let escaped = self.parse_escape()?;
                    let mut buffer = [0u8; 4];
                    out.extend_from_slice(escaped.encode_utf8(&mut buffer).as_bytes());
                }
                byte if byte < 0x20 => {
                    return Err(format!(
                        "unescaped control character 0x{byte:02x} in JSON string"
                    ))
                }
                byte => out.push(byte),
            }
        }
        String::from_utf8(out).map_err(|err| format!("JSON string is not valid UTF-8: {err}"))
    }

    fn parse_escape(&mut self) -> JsonResult<char> {
        match self.bump() {
            Some(b'"') => Ok('"'),
            Some(b'\\') => Ok('\\'),
            Some(b'/') => Ok('/'),
            Some(b'b') => Ok('\u{0008}'),
            Some(b'f') => Ok('\u{000c}'),
            Some(b'n') => Ok('\n'),
            Some(b'r') => Ok('\r'),
            Some(b't') => Ok('\t'),
            Some(b'u') => {
                let first = self.read_hex4()?;
                if (0xDC00..=0xDFFF).contains(&first) {
                    return Err(format!("\\u{first:04X} is a lone UTF-16 low surrogate"));
                }
                if (0xD800..=0xDBFF).contains(&first) {
                    let save = self.pos;
                    if self.peek() == Some(b'\\') && self.bytes.get(self.pos + 1) == Some(&b'u') {
                        self.pos += 2;
                        if let Ok(second) = self.read_hex4() {
                            if (0xDC00..=0xDFFF).contains(&second) {
                                let code = 0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                                if let Some(ch) = char::from_u32(code) {
                                    return Ok(ch);
                                }
                            }
                        }
                        self.pos = save;
                    }
                    return Err(format!("\\u{first:04X} is a lone UTF-16 high surrogate"));
                }
                char::from_u32(first)
                    .ok_or_else(|| format!("invalid Unicode escape \\u{first:04X}"))
            }
            Some(byte) => Err(format!("unknown escape '\\{}' in string", byte as char)),
            None => Err("string escape sequence is incomplete".to_string()),
        }
    }

    fn read_hex4(&mut self) -> JsonResult<u32> {
        let mut value: u32 = 0;
        for _ in 0..4 {
            let Some(byte) = self.bump() else {
                return Err("\\u escape is missing its 4 hex digits".to_string());
            };
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => {
                    return Err(format!(
                        "non-hex character '{}' in \\u escape",
                        byte as char
                    ))
                }
            };
            value = value * 16 + u32::from(digit);
        }
        Ok(value)
    }
}

/// Flatten one lsblk node, recursing into `children`.
fn flatten_node(node: &Json, parent: Option<&str>, out: &mut Vec<BlockDevice>) {
    let Some(name) = node_name(node) else {
        logging::debug("skipped an lsblk device node without a name");
        return;
    };
    let children_nodes = node.get("children").and_then(Json::as_array);
    let mut children: Vec<String> = Vec::new();
    if let Some(items) = children_nodes {
        for item in items {
            if let Some(child) = node_name(item) {
                children.push(child);
            }
        }
    }

    let resolved_parent = parent.map(str::to_string).or_else(|| {
        json_text(node, "pkname")
            .map(|pkname| pkname.strip_prefix("/dev/").unwrap_or(&pkname).to_string())
    });

    out.push(BlockDevice {
        path: format!("/dev/{name}"),
        name: name.clone(),
        dev_type: json_text(node, "type").unwrap_or_else(|| "disk".to_string()),
        fstype: json_text(node, "fstype"),
        label: json_text(node, "label"),
        uuid: json_text(node, "uuid"),
        partuuid: json_text(node, "partuuid"),
        size: json_u64(node, "size"),
        removable: json_bool(node, "rm"),
        read_only: json_bool(node, "ro"),
        hotplug: json_bool(node, "hotplug"),
        transport: json_text(node, "tran"),
        mountpoint: json_text(node, "mountpoint"),
        model: json_text(node, "model"),
        serial: json_text(node, "serial"),
        parent: resolved_parent,
        children,
    });

    if let Some(items) = children_nodes {
        for item in items {
            flatten_node(item, Some(&name), out);
        }
    }
}

/// Device name (also accepts the `/dev/sda` form printed by `lsblk -p`).
fn node_name(node: &Json) -> Option<String> {
    let raw = json_text(node, "name")?;
    Some(raw.strip_prefix("/dev/").unwrap_or(&raw).to_string())
}

/// Read a string field; numbers are turned into text and arrays yield the first non-empty string.
fn json_text(node: &Json, key: &str) -> Option<String> {
    match node.get(key)? {
        Json::Str(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Json::Number(text) => Some(text.clone()),
        Json::Array(items) => items.iter().find_map(|item| {
            let text = item.as_str()?.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }),
        _ => None,
    }
}

/// Boolean fields accept true/false, `"1"`/`"0"` and numbers.
fn json_bool(node: &Json, key: &str) -> bool {
    match node.get(key) {
        Some(Json::Bool(value)) => *value,
        Some(Json::Str(text)) => matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y" | "on"
        ),
        Some(Json::Number(text)) => text.trim() != "0",
        _ => false,
    }
}

/// Size fields: `-b` prints plain bytes, but human-readable forms such as `1.5G` are tolerated.
fn json_u64(node: &Json, key: &str) -> u64 {
    match node.get(key) {
        Some(Json::Number(text)) | Some(Json::Str(text)) => parse_size_text(text),
        _ => 0,
    }
}

fn parse_size_text(text: &str) -> u64 {
    let text = text.trim();
    if let Ok(value) = text.parse::<u64>() {
        return value;
    }
    let mut number = String::new();
    let mut suffix = String::new();
    for ch in text.chars() {
        let is_number_char = ch.is_ascii_digit() || ch == '.';
        if is_number_char && suffix.is_empty() {
            number.push(ch);
        } else {
            suffix.push(ch);
        }
    }
    let Ok(value) = number.parse::<f64>() else {
        return 0;
    };
    let unit = suffix.trim().trim_end_matches('B').trim_end_matches('i');
    let multiplier = match unit.to_ascii_uppercase().as_str() {
        "K" => 1024f64,
        "M" => 1024f64.powi(2),
        "G" => 1024f64.powi(3),
        "T" => 1024f64.powi(4),
        "P" => 1024f64.powi(5),
        "E" => 1024f64.powi(6),
        _ => 1f64,
    };
    (value * multiplier) as u64
}

/// Fill in a parent's children from `pkname` (also useful when lsblk gives no nesting).
fn link_children(devices: &mut [BlockDevice]) {
    let pairs: Vec<(String, String)> = devices
        .iter()
        .filter_map(|device| {
            device
                .parent
                .as_ref()
                .map(|parent| (parent.clone(), device.name.clone()))
        })
        .collect();
    for (parent, child) in pairs {
        if let Some(found) = devices.iter_mut().find(|device| device.name == parent) {
            if !found.children.contains(&child) {
                found.children.push(child);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// lsblk / sysfs / blkid helpers
// ---------------------------------------------------------------------------

/// Run lsblk once and parse it; returns None when the program is missing, exits non-zero, or prints invalid JSON.
fn lsblk_devices(runner: &dyn CommandRunner, columns: &str) -> Option<Vec<BlockDevice>> {
    let output = match runner.run("lsblk", &["-J", "-b", "-o", columns], Some(PROBE_TIMEOUT)) {
        Ok(output) => output,
        Err(err) => {
            logging::debug(format!("failed to run lsblk: {err}"));
            return None;
        }
    };
    if !output.ok() {
        logging::debug(format!(
            "lsblk exited with code {}: {}",
            output.status,
            output.stderr_trimmed()
        ));
        return None;
    }
    match parse_lsblk(&output.stdout) {
        Ok(devices) if !devices.is_empty() => Some(devices),
        Ok(_) => {
            logging::debug("lsblk returned no block devices");
            None
        }
        Err(err) => {
            logging::debug(format!("failed to parse lsblk output: {err}"));
            None
        }
    }
}

/// Enumerate block devices from the sysfs directory (environments or containers without lsblk).
fn list_sysfs(root: &Path, runner: &dyn CommandRunner) -> Vec<BlockDevice> {
    let mut names: Vec<String> = Vec::new();
    match std::fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let Some(name) = file_name.to_str() else {
                    continue;
                };
                if !name.is_empty() {
                    names.push(name.to_string());
                }
            }
        }
        Err(err) => {
            logging::warn(format!("failed to read {}: {err}", root.display()));
            return Vec::new();
        }
    }
    names.sort();

    let mut devices: Vec<BlockDevice> = Vec::new();
    for name in &names {
        let dir = root.join(name);
        let size_sectors = read_trimmed(&dir.join("size"))
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0);
        let removable = read_trimmed(&dir.join("removable"))
            .map(|text| text == "1")
            .unwrap_or(false);
        let read_only = read_trimmed(&dir.join("ro"))
            .map(|text| text == "1")
            .unwrap_or(false);
        let is_partition = dir.join("partition").exists();
        let parent = if is_partition {
            sysfs_parent(&dir, name, &names)
        } else {
            None
        };

        let devnode = format!("/dev/{name}");
        let info = probe_blkid(runner, &devnode);
        let fstype = info
            .get("TYPE")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let label = info
            .get("LABEL")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let uuid = info
            .get("UUID")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let partuuid = info
            .get("PARTUUID")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        devices.push(BlockDevice {
            name: name.clone(),
            path: devnode,
            dev_type: if is_partition { "part" } else { "disk" }.to_string(),
            fstype,
            label,
            uuid,
            partuuid,
            size: size_sectors.saturating_mul(512),
            removable,
            read_only,
            hotplug: removable,
            transport: None,
            mountpoint: None,
            model: None,
            serial: None,
            parent,
            children: Vec::new(),
        });
    }
    link_children(&mut devices);
    inherit_parent_flags(&mut devices);
    devices
}

/// A partition's removable/hotplug attributes live on the parent disk; the sysfs
/// fallback inherits them, otherwise every partition would look like a "built-in
/// disk" and USB partitions would never be auto-mounted.
fn inherit_parent_flags(devices: &mut [BlockDevice]) {
    let mut parents: HashMap<String, (bool, bool)> = HashMap::new();
    for device in devices.iter().filter(|device| device.parent.is_none()) {
        parents.insert(device.name.clone(), (device.removable, device.hotplug));
    }
    for device in devices.iter_mut() {
        let Some(parent) = device.parent.clone() else {
            continue;
        };
        if let Some((removable, hotplug)) = parents.get(&parent) {
            if !device.removable {
                device.removable = *removable;
            }
            if !device.hotplug {
                device.hotplug = *hotplug;
            }
        }
    }
}

/// Parent disk name: prefer the parent directory name of the real path, otherwise infer it from naming rules.
fn sysfs_parent(dir: &Path, name: &str, known: &[String]) -> Option<String> {
    if let Ok(real) = std::fs::canonicalize(dir) {
        if let Some(candidate) = real
            .parent()
            .and_then(Path::file_name)
            .and_then(|file_name| file_name.to_str())
        {
            if candidate != name && known.iter().any(|item| item == candidate) {
                return Some(candidate.to_string());
            }
        }
    }
    guess_parent_name(name)
}

/// Infer the whole-disk name from a partition name: sda1 -> sda, nvme0n1p1 -> nvme0n1, mmcblk0p1 -> mmcblk0.
fn guess_parent_name(name: &str) -> Option<String> {
    if let Some(index) = name.rfind('p') {
        let (head, tail) = name.split_at(index);
        let digits = &tail[1..];
        let head_ends_with_digit = head
            .chars()
            .last()
            .map(|ch| ch.is_ascii_digit())
            .unwrap_or(false);
        if head_ends_with_digit
            && !digits.is_empty()
            && digits.chars().all(|ch| ch.is_ascii_digit())
        {
            return Some(head.to_string());
        }
    }
    let trimmed = name.trim_end_matches(|ch: char| ch.is_ascii_digit());
    if trimmed.is_empty() || trimmed == name {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
}

/// Parse the `KEY=VALUE` output of `blkid -o export`.
fn parse_blkid_export(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        values.insert(key.to_ascii_uppercase(), value.trim().to_string());
    }
    values
}

// ---------------------------------------------------------------------------
// String and matching helpers
// ---------------------------------------------------------------------------

fn is_blank(value: Option<&str>) -> bool {
    value.map(str::trim).map(str::is_empty).unwrap_or(true)
}

fn is_loop_device(device: &BlockDevice) -> bool {
    device.dev_type.eq_ignore_ascii_case("loop") || device.name.starts_with("loop")
}

fn by_uuid<'a>(devices: &'a [BlockDevice], wanted: &str) -> Vec<&'a BlockDevice> {
    let wanted = wanted.trim();
    devices
        .iter()
        .filter(|device| {
            device
                .uuid
                .as_deref()
                .map(|uuid| uuid.trim().eq_ignore_ascii_case(wanted))
                .unwrap_or(false)
        })
        .collect()
}

fn by_partuuid<'a>(devices: &'a [BlockDevice], wanted: &str) -> Vec<&'a BlockDevice> {
    let wanted = wanted.trim();
    devices
        .iter()
        .filter(|device| {
            device
                .partuuid
                .as_deref()
                .map(|partuuid| partuuid.trim().eq_ignore_ascii_case(wanted))
                .unwrap_or(false)
        })
        .collect()
}

fn by_label<'a>(devices: &'a [BlockDevice], wanted: &str) -> Vec<&'a BlockDevice> {
    let wanted = wanted.trim();
    let exact: Vec<&BlockDevice> = devices
        .iter()
        .filter(|device| device.label.as_deref().map(str::trim) == Some(wanted))
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    devices
        .iter()
        .filter(|device| {
            device
                .label
                .as_deref()
                .map(|label| label.trim().eq_ignore_ascii_case(wanted))
                .unwrap_or(false)
        })
        .collect()
}

fn by_name_or_label<'a>(devices: &'a [BlockDevice], reference: &str) -> Vec<&'a BlockDevice> {
    let bare = strip_prefix_ci(reference, "/dev/").unwrap_or(reference);
    let devnode = format!("/dev/{bare}");
    let exact: Vec<&BlockDevice> = devices
        .iter()
        .filter(|device| {
            device.name == reference
                || device.name == bare
                || device.path == reference
                || device.path == devnode
        })
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    by_label(devices, reference)
}

/// Case-insensitive `starts_with` that also returns the remainder.
fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(value[prefix.len()..].trim())
    } else {
        None
    }
}

/// The `\x20` escapes used in `/dev/disk/by-label/My\x20Disk`.
fn unescape_by_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'x') {
            let mut clone = chars.clone();
            clone.next();
            let digits: String = clone.by_ref().take(2).collect();
            if digits.len() == 2 {
                if let Ok(code) = u8::from_str_radix(&digits, 16) {
                    out.push(code as char);
                    chars.next();
                    chars.next();
                    chars.next();
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

fn basename(path: &str) -> &str {
    path.rsplit('/').find(|part| !part.is_empty()).unwrap_or("")
}

fn same_mount_path(left: &str, right: &str) -> bool {
    fn normalize(path: &str) -> &str {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            "/"
        } else {
            trimmed
        }
    }
    normalize(left) == normalize(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use crate::exec::{CommandOutput, MockRunner};
    use crate::mountinfo::parse_mountinfo;
    use crate::testutil::TempDir;

    fn sample_device(name: &str) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            path: format!("/dev/{name}"),
            dev_type: "part".to_string(),
            fstype: None,
            label: None,
            uuid: None,
            partuuid: None,
            size: 0,
            removable: false,
            read_only: false,
            hotplug: false,
            transport: None,
            mountpoint: None,
            model: None,
            serial: None,
            parent: None,
            children: Vec::new(),
        }
    }

    fn library_devices() -> Vec<BlockDevice> {
        let mut sda = sample_device("sda");
        sda.dev_type = "disk".to_string();
        sda.size = 500_107_862_016;
        sda.children = vec!["sda1".to_string(), "sda2".to_string()];

        let mut sda1 = sample_device("sda1");
        sda1.fstype = Some("ext4".to_string());
        sda1.uuid = Some("SYSTEM-0001".to_string());
        sda1.partuuid = Some("part-sda1".to_string());
        sda1.parent = Some("sda".to_string());
        sda1.mountpoint = Some("/".to_string());
        sda1.size = 64 * 1024 * 1024 * 1024;

        let mut sda2 = sample_device("sda2");
        sda2.fstype = Some("vfat".to_string());
        sda2.uuid = Some("SYSTEM-0002".to_string());
        sda2.parent = Some("sda".to_string());
        sda2.size = 512 * 1024 * 1024;

        let mut sdb = sample_device("sdb");
        sdb.dev_type = "disk".to_string();
        sdb.removable = true;
        sdb.hotplug = true;
        sdb.transport = Some("usb".to_string());
        sdb.children = vec!["sdb1".to_string()];
        sdb.size = 32 * 1024 * 1024 * 1024;

        let mut sdb1 = sample_device("sdb1");
        sdb1.fstype = Some("exfat".to_string());
        sdb1.uuid = Some("ABCD-1234".to_string());
        sdb1.partuuid = Some("part-sdb1".to_string());
        sdb1.label = Some("Café Disk".to_string());
        sdb1.parent = Some("sdb".to_string());
        sdb1.removable = true;
        sdb1.size = 31 * 1024 * 1024 * 1024;

        vec![sda, sda1, sda2, sdb, sdb1]
    }

    fn write_file(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create directory");
        }
        std::fs::write(path, text).expect("failed to write file");
    }

    #[test]
    fn parses_nested_lsblk_json() {
        let text = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","size":500107862016,"rm":false,"ro":false,
             "hotplug":false,"tran":"sata","model":"SSD","serial":"S1",
             "children":[
               {"name":"sda1","type":"part","fstype":"ext4","uuid":"AAAA-1111",
                "partuuid":"pppp-1","size":536870912,"rm":"0","ro":"0","hotplug":"0",
                "pkname":"sda","mountpoint":"/"}
             ]},
            {"name":"sdb","type":"disk","rm":"1","hotplug":"1","tran":"usb","size":"1.5G",
             "children":[
               {"name":"sdb1","type":"part","fstype":"exfat","label":"Café Disk",
                "uuid":"ABCD-1234","size":"1600000000","rm":"1","ro":"0","hotplug":"1",
                "pkname":"sdb"}
             ]}
          ]
        }"#;

        let devices = parse_lsblk(text).unwrap();
        assert_eq!(devices.len(), 4);

        let sda = devices.iter().find(|d| d.name == "sda").unwrap();
        assert_eq!(sda.path, "/dev/sda");
        assert_eq!(sda.dev_type, "disk");
        assert_eq!(sda.size, 500_107_862_016);
        assert_eq!(sda.transport.as_deref(), Some("sata"));
        assert_eq!(sda.model.as_deref(), Some("SSD"));
        assert_eq!(sda.children, vec!["sda1"]);
        assert!(sda.fstype.is_none());

        let sda1 = devices.iter().find(|d| d.name == "sda1").unwrap();
        assert_eq!(sda1.parent.as_deref(), Some("sda"));
        assert_eq!(sda1.mountpoint.as_deref(), Some("/"));
        assert_eq!(sda1.partuuid.as_deref(), Some("pppp-1"));
        assert_eq!(sda1.size, 536_870_912);
        assert!(sda1.is_partition());

        let sdb = devices.iter().find(|d| d.name == "sdb").unwrap();
        assert!(sdb.removable);
        assert!(sdb.hotplug);
        assert_eq!(sdb.size, 1_610_612_736);

        let sdb1 = devices.iter().find(|d| d.name == "sdb1").unwrap();
        assert_eq!(sdb1.label.as_deref(), Some("Café Disk"));
        // "0" must parse as false
        assert!(!sdb1.read_only);
    }

    #[test]
    fn parses_json_escapes_and_unicode() {
        let text = r#"{"blockdevices":[
            {"name":"sdc","type":"disk",
             "label":"Caf\u00e9 \/ \"A\"\tB",
             "model":"Disk\\Pro",
             "serial":"\ud83d\ude00"}
        ]}"#;
        let devices = parse_lsblk(text).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].label.as_deref(), Some("Café / \"A\"\tB"));
        assert_eq!(devices[0].model.as_deref(), Some("Disk\\Pro"));
        assert_eq!(devices[0].serial.as_deref(), Some("😀"));
    }

    #[test]
    fn rejects_invalid_json() {
        let empty = parse_lsblk("  ").unwrap_err();
        assert_eq!(empty.kind, ErrorKind::Device);
        assert!(empty.message.contains("lsblk"));

        for bad in [
            "[",
            "{\"blockdevices\": [}",
            "{\"blockdevices\": {}}",
            "{\"other\": []}",
            "{\"blockdevices\": [{\"name\":}]}",
            "not json",
            "{\"blockdevices\": [{\"name\":\"sda\"}]} trailing",
        ] {
            assert!(parse_lsblk(bad).is_err(), "should fail to parse: {bad}");
        }
    }

    #[test]
    fn lists_devices_via_lsblk_and_filters_loop() {
        let json = r#"{"blockdevices":[
            {"name":"loop0","type":"loop","size":1024},
            {"name":"sdb","type":"disk","size":16000000000,"rm":"1","children":[
                {"name":"sdb1","type":"part","fstype":"exfat","label":"USB",
                 "size":15000000000,"rm":1,"pkname":"sdb"}
            ]}
        ]}"#;

        let runner = MockRunner::new().respond("lsblk", CommandOutput::success(json));
        let devices = list_devices(&runner, false, true);
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["sdb", "sdb1"]);

        let calls = runner.calls();
        let lsblk_call = calls
            .iter()
            .find(|(program, _)| program == "lsblk")
            .expect("lsblk should have been called");
        assert_eq!(lsblk_call.1, vec!["-J", "-b", "-o", FULL_COLUMNS]);

        // include_loop = true keeps loop devices
        let runner = MockRunner::new().respond("lsblk", CommandOutput::success(json));
        let devices = list_devices(&runner, true, true);
        assert!(devices.iter().any(|d| d.name == "loop0"));

        // prefer_lsblk = false never calls lsblk
        let runner = MockRunner::new();
        let _ = list_devices(&runner, true, false);
        assert!(!runner.called("lsblk"));
    }

    #[test]
    fn falls_back_to_minimal_columns_and_sysfs() {
        let runner = MockRunner::new()
            .respond("lsblk", CommandOutput::failure(1, "lsblk: unknown column"))
            .respond("lsblk", CommandOutput::failure(1, "lsblk: unknown column"));
        let devices = list_devices(&runner, false, true);
        // On a dev machine /sys/class/block may not exist, so the result is empty, but
        // lsblk must still have been tried twice
        let lsblk_calls = runner
            .calls()
            .iter()
            .filter(|(program, _)| program == "lsblk")
            .count();
        assert_eq!(lsblk_calls, 2);
        assert!(devices.iter().all(|device| !is_loop_device(device)));
    }

    #[test]
    fn sysfs_fallback_reads_attributes() {
        let temp = TempDir::new("devices-sysfs");
        let root = temp.join("block");
        write_file(&root.join("sda").join("size"), "1000000\n");
        write_file(&root.join("sda").join("removable"), "0\n");
        write_file(&root.join("sda").join("ro"), "0\n");
        write_file(&root.join("sda1").join("partition"), "1\n");
        write_file(&root.join("sda1").join("size"), "204800\n");
        write_file(&root.join("sda1").join("removable"), "1\n");
        write_file(&root.join("sda1").join("ro"), "1\n");

        let runner = MockRunner::new()
            .respond("blkid", CommandOutput::success(""))
            .respond(
                "blkid",
                CommandOutput::success(
                    "DEVNAME=/dev/sda1\nTYPE=exfat\nUUID=ABCD-1234\nLABEL=USB\nPARTUUID=p-1\n",
                ),
            );

        let devices = list_sysfs(&root, &runner);
        assert_eq!(devices.len(), 2);

        let disk = devices.iter().find(|d| d.name == "sda").unwrap();
        assert_eq!(disk.dev_type, "disk");
        assert_eq!(disk.size, 1_000_000 * 512);
        assert_eq!(disk.children, vec!["sda1"]);

        let part = devices.iter().find(|d| d.name == "sda1").unwrap();
        assert_eq!(part.dev_type, "part");
        assert_eq!(part.path, "/dev/sda1");
        assert_eq!(part.parent.as_deref(), Some("sda"));
        assert_eq!(part.size, 204_800 * 512);
        assert_eq!(part.fstype.as_deref(), Some("exfat"));
        assert_eq!(part.label.as_deref(), Some("USB"));
        assert_eq!(part.uuid.as_deref(), Some("ABCD-1234"));
        assert_eq!(part.partuuid.as_deref(), Some("p-1"));
        assert!(part.removable);
        assert!(part.read_only);
    }

    #[test]
    fn probes_blkid_and_refreshes_device() {
        let runner = MockRunner::new().respond(
            "blkid",
            CommandOutput::success(
                "DEVNAME=/dev/sdb1\nTYPE=exfat\nUUID=ABCD-1234\nLABEL=My Disk\n",
            ),
        );
        let info = probe_blkid(&runner, "/dev/sdb1");
        assert_eq!(info.get("TYPE").map(String::as_str), Some("exfat"));
        assert_eq!(info.get("LABEL").map(String::as_str), Some("My Disk"));
        assert!(runner.called("blkid"));

        // Without blkid the map is empty and no command is run
        let runner = MockRunner::new().missing("blkid");
        assert!(probe_blkid(&runner, "/dev/sdb1").is_empty());
        assert!(!runner.called("blkid"));

        // A failing blkid (device without a filesystem) also yields an empty map
        let runner = MockRunner::new().respond("blkid", CommandOutput::failure(2, ""));
        assert!(probe_blkid(&runner, "/dev/sdb1").is_empty());

        // Missing filesystem information triggers a re-probe
        let mut device = sample_device("sdb1");
        let runner = MockRunner::new().respond(
            "blkid",
            CommandOutput::success("TYPE=ntfs\nUUID=1234-ABCD\nLABEL=Win\n"),
        );
        let refreshed = refresh_device(&device, &runner);
        assert_eq!(refreshed.fstype.as_deref(), Some("ntfs"));
        assert_eq!(refreshed.uuid.as_deref(), Some("1234-ABCD"));
        assert_eq!(refreshed.label.as_deref(), Some("Win"));
        assert!(runner.called("blkid"));

        // Complete information means no repeated probing
        device.fstype = Some("ext4".to_string());
        device.uuid = Some("FULL".to_string());
        device.label = Some("Data".to_string());
        let runner = MockRunner::new();
        let untouched = refresh_device(&device, &runner);
        assert_eq!(untouched, device);
        assert!(!runner.called("blkid"));
    }

    #[test]
    fn resolves_device_references() {
        let devices = library_devices();

        let cases = [
            ("sdb1", "sdb1"),
            ("/dev/sdb1", "sdb1"),
            ("UUID=abcd-1234", "sdb1"),
            ("uuid:ABCD-1234", "sdb1"),
            ("/dev/disk/by-uuid/ABCD-1234", "sdb1"),
            ("PARTUUID=part-sdb1", "sdb1"),
            ("LABEL=Café Disk", "sdb1"),
            ("Café Disk", "sdb1"),
            ("PARTUUID=part-sda1", "sda1"),
            ("sda2", "sda2"),
            ("/dev/disk/by-label/Café Disk", "sdb1"),
        ];
        for (reference, expected) in cases {
            let device = resolve_device(&devices, reference)
                .unwrap_or_else(|err| panic!("failed to resolve {reference}: {err}"));
            assert_eq!(device.name, expected, "reference {reference}");
        }

        let err = resolve_device(&devices, "zzz").unwrap_err();
        assert!(err.message.contains("device not found"));
        assert!(err
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("automountd list --all"));

        let err = resolve_device(&devices, "  ").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Device);
    }

    #[test]
    fn reports_ambiguous_references() {
        let mut devices = library_devices();
        let mut clone = sample_device("sdc1");
        clone.fstype = Some("exfat".to_string());
        clone.label = Some("Café Disk".to_string());
        clone.parent = Some("sdc".to_string());
        devices.push(clone);

        let err = resolve_device(&devices, "LABEL=Café Disk").unwrap_err();
        assert!(err.message.contains("matches 2 devices"), "{}", err.message);
        assert!(err.message.contains("/dev/sdb1"));
        assert!(err.message.contains("/dev/sdc1"));
        assert!(err.hint.is_some());

        // An exact device name is unaffected by label ambiguity
        assert_eq!(resolve_device(&devices, "sdc1").unwrap().path, "/dev/sdc1");
    }

    #[test]
    fn finds_names_and_tracks_top_level() {
        let devices = library_devices();
        assert_eq!(find_by_name(&devices, "sdb1").unwrap().path, "/dev/sdb1");
        assert_eq!(find_by_name(&devices, "/dev/sdb1").unwrap().name, "sdb1");
        assert!(find_by_name(&devices, "nope").is_none());
        assert!(find_by_name(&devices, "  ").is_none());

        let part = find_by_name(&devices, "sdb1").unwrap();
        assert_eq!(top_level_name(&devices, &part), "sdb");
        let disk = find_by_name(&devices, "sdb").unwrap();
        assert_eq!(top_level_name(&devices, &disk), "sdb");

        // A parent/child cycle must always terminate
        let mut first = sample_device("sdx");
        first.parent = Some("sdy".to_string());
        let mut second = sample_device("sdy");
        second.parent = Some("sdx".to_string());
        let cyclic = vec![first, second];
        let top = top_level_name(&cyclic, &cyclic[0]);
        assert!(top == "sdx" || top == "sdy", "unexpected result: {top}");
    }

    #[test]
    fn unescapes_labels_and_parses_sizes() {
        assert_eq!(unescape_by_label(r"My\x20Disk"), "My Disk");
        assert_eq!(unescape_by_label("plain"), "plain");

        assert_eq!(parse_size_text("2048"), 2048);
        assert_eq!(parse_size_text("1.5G"), 1_610_612_736);
        assert_eq!(parse_size_text("512M"), 536_870_912);
        assert_eq!(parse_size_text("nope"), 0);

        assert_eq!(guess_parent_name("sda1").as_deref(), Some("sda"));
        assert_eq!(guess_parent_name("nvme0n1p2").as_deref(), Some("nvme0n1"));
        assert_eq!(guess_parent_name("mmcblk0p1").as_deref(), Some("mmcblk0"));
        assert_eq!(guess_parent_name("sda"), None);

        assert_eq!(basename("/dev/sdb1"), "sdb1");
        assert_eq!(basename("/dev/"), "dev");
        assert_eq!(basename("/"), "");
        assert!(same_mount_path("/boot/efi/", "/boot/efi"));
        assert!(!same_mount_path("/boot", "/boot/efi"));
    }

    #[test]
    fn detects_system_disks_from_mountinfo() {
        let mut devices = library_devices();
        let mut sdc = sample_device("sdc");
        sdc.dev_type = "disk".to_string();
        sdc.children = vec!["sdc1".to_string()];
        let mut sdc1 = sample_device("sdc1");
        sdc1.parent = Some("sdc".to_string());
        sdc1.fstype = Some("exfat".to_string());
        devices.push(sdc);
        devices.push(sdc1);

        let mounts = parse_mountinfo(concat!(
            "21 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n",
            "22 21 8:2 / /boot rw,relatime - ext4 /dev/sda2 rw\n",
            "23 21 8:3 / /boot/efi/ rw - vfat /dev/sda2 rw\n",
            "24 21 0:4 / /proc rw - proc proc rw\n",
            "25 21 8:5 / /home rw - exfat /dev/sdc1 rw\n",
        ));
        let names = system_disk_names(&devices, &mounts);
        assert!(
            names.contains("sda"),
            "system disks should contain sda: {names:?}"
        );
        assert!(
            !names.contains("sdc"),
            "data disk must not count as a system disk"
        );
        assert!(!names.contains("proc"));

        // An empty mount table yields no system disks
        assert!(system_disk_names(&devices, &[]).is_empty());
    }
}
