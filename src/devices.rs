//! 块设备发现：优先 `lsblk -J`，否则回退 sysfs + blkid。
//!
//! `lsblk -J` 输出 JSON，但工程不允许第三方依赖，因此本模块自带一个
//! 只覆盖 lsblk 输出所需语法的极简 JSON 解析器：object / array / string /
//! number / true / false / null；字符串支持 `\" \\ \/ \b \f \n \r \t` 与
//! `\uXXXX`（含 UTF-16 代理对）。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::exec::CommandRunner;
use crate::logging;
use crate::model::{BlockDevice, MountEntry};

/// 完整列集合（较新的 util-linux）。
pub const FULL_COLUMNS: &str =
    "NAME,TYPE,FSTYPE,LABEL,UUID,PARTUUID,SIZE,RM,RO,HOTPLUG,TRAN,MOUNTPOINT,MODEL,SERIAL,PKNAME";
/// 兼容旧版 util-linux 的最小列集合。
pub const MINIMAL_COLUMNS: &str = "NAME,TYPE,FSTYPE,LABEL,UUID,SIZE,RM,RO,MOUNTPOINT";

/// sysfs 中的块设备目录。
const SYSFS_BLOCK_DIR: &str = "/sys/class/block";
/// 探测命令超时，避免无响应设备卡住守护进程。
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// 列出当前所有块设备。
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
            "lsblk 不可用或输出无法解析，改用 {SYSFS_BLOCK_DIR} 枚举块设备"
        ));
        devices = list_sysfs(Path::new(SYSFS_BLOCK_DIR), runner);
    }

    if !include_loop {
        devices.retain(|device| !is_loop_device(device));
    }
    devices
}

/// 解析 `lsblk -J` 输出（嵌套 JSON 展平）。
pub fn parse_lsblk(text: &str) -> Result<Vec<BlockDevice>> {
    let text = text.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return Err(Error::device("lsblk 输出为空，无法解析设备列表")
            .with_hint("请确认 util-linux 已正确安装（lsblk -J 应输出 JSON）"));
    }
    let document = JsonParser::new(text).parse_document().map_err(|reason| {
        Error::device(format!("解析 lsblk JSON 失败: {reason}"))
            .with_hint("请确认 lsblk 来自 util-linux 2.27 或更新版本")
    })?;

    let Some(blockdevices) = document.get("blockdevices") else {
        return Err(Error::device("lsblk JSON 中缺少 blockdevices 字段")
            .with_hint("请确认命令为 'lsblk -J -b -o ...'"));
    };
    let Some(items) = blockdevices.as_array() else {
        return Err(Error::device("lsblk JSON 中 blockdevices 不是数组"));
    };

    let mut devices = Vec::new();
    for item in items {
        flatten_node(item, None, &mut devices);
    }
    link_children(&mut devices);
    Ok(devices)
}

/// 用 blkid 读取文件系统信息；失败或无文件系统时返回空表。
pub fn probe_blkid(runner: &dyn CommandRunner, device_path: &str) -> BTreeMap<String, String> {
    if !runner.which("blkid") {
        logging::debug("未找到 blkid，跳过文件系统探测");
        return BTreeMap::new();
    }
    let output = match runner.run("blkid", &["-o", "export", device_path], Some(PROBE_TIMEOUT)) {
        Ok(output) => output,
        Err(err) => {
            logging::debug(format!("探测 {device_path} 失败: {err}"));
            return BTreeMap::new();
        }
    };
    if !output.ok() {
        // 没有文件系统的设备 blkid 返回非 0，属于正常情况。
        logging::debug(format!(
            "blkid 未能在 {device_path} 上找到文件系统（退出码 {}）",
            output.status
        ));
        return BTreeMap::new();
    }
    parse_blkid_export(&output.stdout)
}

/// 重新探测文件系统（udev 事件到达时分区可能尚未就绪）。
pub fn refresh_device(device: &BlockDevice, runner: &dyn CommandRunner) -> BlockDevice {
    let mut updated = device.clone();
    let missing_fstype = is_blank(updated.fstype.as_deref());
    let missing_uuid = is_blank(updated.uuid.as_deref());
    if !missing_fstype && !missing_uuid {
        return updated;
    }

    let info = probe_blkid(runner, &updated.path);
    if info.is_empty() {
        logging::debug(format!("{} 仍无法探测到文件系统信息", updated.path));
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

/// 按 设备名 / 路径 / UUID / PARTUUID / 卷标 查找设备。
///
/// 支持 `sdb1`、`/dev/sdb1`、`UUID=xxxx`、`/dev/disk/by-uuid/xxxx`、
/// `LABEL=备份盘`、`备份盘` 等写法。
pub fn resolve_device(devices: &[BlockDevice], reference: &str) -> Result<BlockDevice> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(Error::device("设备引用不能为空")
            .with_hint("可用 UUID=、/dev/sdb1、/dev/disk/by-uuid/... 或卷标指定设备"));
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
        [] => Err(Error::device(format!("找不到设备 {reference}")).with_hint(
            "运行 'automountd list --all' 查看当前可用的块设备；也可用 UUID= 或完整设备路径指定",
        )),
        [only] => Ok((*only).clone()),
        many => {
            let candidates: Vec<String> = many
                .iter()
                .map(|device| {
                    let label = device.label.as_deref().unwrap_or("-");
                    format!("{}（{}，卷标 {label}）", device.path, device.dev_type)
                })
                .collect();
            Err(Error::device(format!(
                "设备引用 {reference} 匹配到 {} 个设备: {}",
                many.len(),
                candidates.join("、")
            ))
            .with_hint("请改用 UUID=... 或完整的设备路径来精确指定设备"))
        }
    }
}

/// 按设备名查找。
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

/// 向上追到整盘名（sda1 -> sda）。
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
                "设备 {} 的父子关系存在环，已停止向上追溯",
                device.name
            ));
            break;
        }
        if devices.iter().any(|candidate| candidate.name == parent) {
            current = parent.to_string();
            continue;
        }
        // 父设备不在列表里（例如只有分区信息），父名本身就是整盘名。
        return parent.to_string();
    }
    current
}

/// 找出承载系统关键目录（/、/boot、/usr 等）的整盘名。
pub fn system_disk_names(devices: &[BlockDevice], mounts: &[MountEntry]) -> HashSet<String> {
    /// 系统关键挂载点：这些目录所在的磁盘绝不自动挂载。
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
        // /dev/disk/by-uuid/... 这类软链接需要先解析成真实设备节点。
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
            // 设备不在当前列表中（容器、权限受限等），至少保护已知的节点名。
            logging::debug(format!(
                "系统挂载点 {} 的设备 {} 不在设备列表中，按节点名保护",
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
// lsblk JSON 解析
// ---------------------------------------------------------------------------

type JsonResult<T> = std::result::Result<T, String>;

/// 极简 JSON 值。
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

/// 手写 JSON 解析器（只解析，不做序列化）。
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
            return Err(format!("JSON 末尾存在多余内容（偏移 {}）", self.pos));
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
                "偏移 {} 处期望 '{}'，实际为 '{}'",
                self.pos - 1,
                expected as char,
                byte as char
            )),
            None => Err(format!("JSON 意外结束，期望 '{}'", expected as char)),
        }
    }

    fn parse_value(&mut self, depth: usize) -> JsonResult<Json> {
        if depth > 96 {
            return Err("JSON 嵌套层级过深".to_string());
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
                "偏移 {} 处出现非法字符 '{}'",
                self.pos, byte as char
            )),
            None => Err("JSON 在读取值时意外结束".to_string()),
        }
    }

    fn parse_literal(&mut self, literal: &str, value: Json) -> JsonResult<Json> {
        let end = self.pos + literal.len();
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| format!("偏移 {} 处的字面量不完整", self.pos))?;
        if slice != literal.as_bytes() {
            return Err(format!("偏移 {} 处期望字面量 {literal}", self.pos));
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
            .map_err(|err| format!("偏移 {start} 处的数字不是合法 UTF-8: {err}"))?;
        if text.is_empty() || text.parse::<f64>().is_err() {
            return Err(format!("偏移 {start} 处的数字非法: {text:?}"));
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
                        "偏移 {} 处期望 ',' 或 ']'，实际为 '{}'",
                        self.pos - 1,
                        byte as char
                    ))
                }
                None => return Err("JSON 数组未闭合".to_string()),
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
                return Err(format!("偏移 {} 处对象的键必须是字符串", self.pos));
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
                        "偏移 {} 处期望 ',' 或 '}}'，实际为 '{}'",
                        self.pos - 1,
                        byte as char
                    ))
                }
                None => return Err("JSON 对象未闭合".to_string()),
            }
        }
        Ok(Json::Object(pairs))
    }

    fn parse_string(&mut self) -> JsonResult<String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.bump() else {
                return Err("JSON 字符串未闭合".to_string());
            };
            match byte {
                b'"' => break,
                b'\\' => {
                    let escaped = self.parse_escape()?;
                    let mut buffer = [0u8; 4];
                    out.extend_from_slice(escaped.encode_utf8(&mut buffer).as_bytes());
                }
                byte if byte < 0x20 => {
                    return Err(format!("JSON 字符串中出现未转义的控制字符 0x{byte:02x}"))
                }
                byte => out.push(byte),
            }
        }
        String::from_utf8(out).map_err(|err| format!("JSON 字符串不是合法 UTF-8: {err}"))
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
                    return Err(format!("\\u{first:04X} 是孤立的 UTF-16 低代理项"));
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
                    return Err(format!("\\u{first:04X} 是孤立的 UTF-16 高代理项"));
                }
                char::from_u32(first).ok_or_else(|| format!("无效的 Unicode 转义 \\u{first:04X}"))
            }
            Some(byte) => Err(format!("字符串中出现未知转义 '\\{}'", byte as char)),
            None => Err("字符串转义序列不完整".to_string()),
        }
    }

    fn read_hex4(&mut self) -> JsonResult<u32> {
        let mut value: u32 = 0;
        for _ in 0..4 {
            let Some(byte) = self.bump() else {
                return Err("\\u 转义缺少 4 位十六进制数字".to_string());
            };
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(format!("\\u 转义中出现非十六进制字符 '{}'", byte as char)),
            };
            value = value * 16 + u32::from(digit);
        }
        Ok(value)
    }
}

/// 展平一个 lsblk 节点，递归处理 `children`。
fn flatten_node(node: &Json, parent: Option<&str>, out: &mut Vec<BlockDevice>) {
    let Some(name) = node_name(node) else {
        logging::debug("lsblk 输出中存在缺少 name 的设备节点，已跳过");
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

/// 设备名（兼容 `lsblk -p` 输出的 `/dev/sda` 形式）。
fn node_name(node: &Json) -> Option<String> {
    let raw = json_text(node, "name")?;
    Some(raw.strip_prefix("/dev/").unwrap_or(&raw).to_string())
}

/// 读取字符串字段；数字也转成文本，数组取第一个非空字符串。
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

/// 布尔字段兼容 true/false、`"1"`/`"0"` 与数字。
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

/// 大小字段：`-b` 输出纯字节数，同时容忍 `1.5G` 这类人类可读写法。
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

/// 用 `pkname` 补齐父设备的 children（lsblk 未给出嵌套结构时也有用）。
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
// lsblk / sysfs / blkid 辅助
// ---------------------------------------------------------------------------

/// 执行一次 lsblk 并解析；命令不存在、退出码非 0 或 JSON 非法时返回 None。
fn lsblk_devices(runner: &dyn CommandRunner, columns: &str) -> Option<Vec<BlockDevice>> {
    let output = match runner.run("lsblk", &["-J", "-b", "-o", columns], Some(PROBE_TIMEOUT)) {
        Ok(output) => output,
        Err(err) => {
            logging::debug(format!("执行 lsblk 失败: {err}"));
            return None;
        }
    };
    if !output.ok() {
        logging::debug(format!(
            "lsblk 退出码 {}：{}",
            output.status,
            output.stderr_trimmed()
        ));
        return None;
    }
    match parse_lsblk(&output.stdout) {
        Ok(devices) if !devices.is_empty() => Some(devices),
        Ok(_) => {
            logging::debug("lsblk 没有返回任何块设备");
            None
        }
        Err(err) => {
            logging::debug(format!("lsblk 输出无法解析: {err}"));
            None
        }
    }
}

/// 从 sysfs 目录枚举块设备（无 lsblk 的环境或容器）。
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
            logging::warn(format!("无法读取 {}: {err}", root.display()));
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

/// 分区的 removable/hotplug 属性在父盘上；sysfs 兜底时把它们继承下来，
/// 否则所有分区都会被当作“内置盘”，USB 分区永远不会自动挂载。
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

/// 父盘名：优先取真实路径的上一级目录名，否则按名称规则推断。
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

/// 从分区名推断整盘名：sda1 -> sda、nvme0n1p1 -> nvme0n1、mmcblk0p1 -> mmcblk0。
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

/// 解析 `blkid -o export` 的 `KEY=VALUE` 输出。
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
// 字符串与匹配辅助
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

/// `starts_with` 的大小写不敏感版本，返回剩余部分。
fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(value[prefix.len()..].trim())
    } else {
        None
    }
}

/// `/dev/disk/by-label/My\x20Disk` 里的 `\x20` 转义。
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
        sdb1.label = Some("备份盘".to_string());
        sdb1.parent = Some("sdb".to_string());
        sdb1.removable = true;
        sdb1.size = 31 * 1024 * 1024 * 1024;

        vec![sda, sda1, sda2, sdb, sdb1]
    }

    fn write_file(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建目录失败");
        }
        std::fs::write(path, text).expect("写入文件失败");
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
               {"name":"sdb1","type":"part","fstype":"exfat","label":"备份盘",
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
        assert_eq!(sdb1.label.as_deref(), Some("备份盘"));
        // "0" 必须解析成 false
        assert!(!sdb1.read_only);
    }

    #[test]
    fn parses_json_escapes_and_unicode() {
        let text = r#"{"blockdevices":[
            {"name":"sdc","type":"disk",
             "label":"\u5907\u4efd\u76d8 \/ \"A\"\tB",
             "model":"Disk\\Pro",
             "serial":"\ud83d\ude00"}
        ]}"#;
        let devices = parse_lsblk(text).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].label.as_deref(), Some("备份盘 / \"A\"\tB"));
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
            assert!(parse_lsblk(bad).is_err(), "应当解析失败: {bad}");
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
            .expect("应当调用 lsblk");
        assert_eq!(lsblk_call.1, vec!["-J", "-b", "-o", FULL_COLUMNS]);

        // include_loop = true 时保留 loop 设备
        let runner = MockRunner::new().respond("lsblk", CommandOutput::success(json));
        let devices = list_devices(&runner, true, true);
        assert!(devices.iter().any(|d| d.name == "loop0"));

        // prefer_lsblk = false 时不调用 lsblk
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
        // 开发机上 /sys/class/block 不存在时结果为空，但必须尝试过两次 lsblk
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

        // 没有 blkid 时返回空表且不执行命令
        let runner = MockRunner::new().missing("blkid");
        assert!(probe_blkid(&runner, "/dev/sdb1").is_empty());
        assert!(!runner.called("blkid"));

        // blkid 失败（设备无文件系统）也返回空表
        let runner = MockRunner::new().respond("blkid", CommandOutput::failure(2, ""));
        assert!(probe_blkid(&runner, "/dev/sdb1").is_empty());

        // 缺少文件系统信息时重新探测
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

        // 信息齐全时不重复探测
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
            ("LABEL=备份盘", "sdb1"),
            ("备份盘", "sdb1"),
            ("PARTUUID=part-sda1", "sda1"),
            ("sda2", "sda2"),
            ("/dev/disk/by-label/备份盘", "sdb1"),
        ];
        for (reference, expected) in cases {
            let device = resolve_device(&devices, reference)
                .unwrap_or_else(|err| panic!("解析 {reference} 失败: {err}"));
            assert_eq!(device.name, expected, "引用 {reference}");
        }

        let err = resolve_device(&devices, "zzz").unwrap_err();
        assert!(err.message.contains("找不到设备"));
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
        clone.label = Some("备份盘".to_string());
        clone.parent = Some("sdc".to_string());
        devices.push(clone);

        let err = resolve_device(&devices, "LABEL=备份盘").unwrap_err();
        assert!(err.message.contains("匹配到 2 个设备"), "{}", err.message);
        assert!(err.message.contains("/dev/sdb1"));
        assert!(err.message.contains("/dev/sdc1"));
        assert!(err.hint.is_some());

        // 精确的设备名不受卷标歧义影响
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

        // 父子关系成环时必须终止
        let mut first = sample_device("sdx");
        first.parent = Some("sdy".to_string());
        let mut second = sample_device("sdy");
        second.parent = Some("sdx".to_string());
        let cyclic = vec![first, second];
        let top = top_level_name(&cyclic, &cyclic[0]);
        assert!(top == "sdx" || top == "sdy", "意外的结果: {top}");
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
        assert!(names.contains("sda"), "系统盘应包含 sda: {names:?}");
        assert!(!names.contains("sdc"), "数据盘不应被当成系统盘");
        assert!(!names.contains("proc"));

        // 空挂载表不产生系统盘
        assert!(system_disk_names(&devices, &[]).is_empty());
    }
}
