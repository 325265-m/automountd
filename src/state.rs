//! 运行期状态：已挂载记录 + 守护进程元信息。
//!
//! 文件格式为带注释的 TSV（`<state_dir>/state.tsv`），权限 0644，
//! 供非 root 用户执行 `automountd status/list` 时读取。
//!
//! ```text
//! # automountd state v1
//! pid=1234
//! started_at=1700000000
//! dry_run=false
//! backend=udev
//! mount\t<device>\t<target>\t<fstype>\t<driver>\t<uuid>\t<label>\t<owner_uid>\t<reason>\t<mounted_at>\t<options>
//! ```
//!
//! 所有字段都做百分号转义，避免卷标里的 TAB / 换行破坏行结构。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::config::Config;
use crate::error::{Error, ErrorKind, Result};
use crate::logging;
use crate::model::MountRecord;

const STATE_HEADER: &str = "# automountd state v1";
const STATE_FILE: &str = "state.tsv";

#[derive(Debug, Clone)]
pub struct State {
    pub path: PathBuf,
    pub records: BTreeMap<String, MountRecord>,
    pub pid: u32,
    pub started_at: u64,
    pub dry_run: bool,
    pub backend: String,
}

impl State {
    pub fn new(config: &Config) -> Self {
        Self {
            path: config.state_dir.join(STATE_FILE),
            records: BTreeMap::new(),
            // 只有守护进程启动时才写入 pid/时间；CLI 读取不到文件时不应伪装成“上次运行”。
            pid: 0,
            started_at: 0,
            dry_run: config.dry_run,
            backend: String::new(),
        }
    }

    /// 创建状态目录（0755）。
    pub fn prepare(&self) -> Result<()> {
        let dir = self.directory();
        fs::create_dir_all(&dir).map_err(|err| {
            Error::new(
                ErrorKind::Io,
                format!("无法创建状态目录 {}: {err}", dir.display()),
            )
            .with_hint("请确认 automountd 有写入权限，或修改 state_dir 配置")
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
        }
        Ok(())
    }

    /// 读取磁盘上的状态，文件缺失不算错误。
    pub fn load(&mut self) -> Result<()> {
        self.records.clear();
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("无法读取状态文件 {}: {err}", self.path.display()),
                ))
            }
        };
        for (index, line) in text.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("mount\t") {
                match parse_mount_line(rest) {
                    Some(record) => {
                        self.records.insert(record.device.clone(), record);
                    }
                    None => logging::warn(format!(
                        "状态文件第 {} 行的挂载记录已损坏，已跳过",
                        index + 1
                    )),
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "pid" => self.pid = value.parse().unwrap_or(0),
                "started_at" => self.started_at = value.parse().unwrap_or(0),
                "dry_run" => {
                    self.dry_run = matches!(
                        value.to_ascii_lowercase().as_str(),
                        "true" | "1" | "yes" | "on"
                    )
                }
                "backend" => self.backend = decode_field(value),
                _ => {}
            }
        }
        Ok(())
    }

    /// 原子写入状态文件。
    pub fn save(&self) -> Result<()> {
        let dir = self.directory();
        fs::create_dir_all(&dir).map_err(|err| {
            Error::new(
                ErrorKind::Io,
                format!("无法创建状态目录 {}: {err}", dir.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
        }

        let mut text = String::new();
        text.push_str(STATE_HEADER);
        text.push('\n');
        text.push_str(&format!("pid={}\n", self.pid));
        text.push_str(&format!("started_at={}\n", self.started_at));
        text.push_str(&format!(
            "dry_run={}\n",
            if self.dry_run { "true" } else { "false" }
        ));
        text.push_str(&format!("backend={}\n", encode_field(&self.backend)));
        for record in self.records.values() {
            text.push_str("mount\t");
            text.push_str(&encode_mount_line(record));
            text.push('\n');
        }

        let tmp = dir.join(format!(".{STATE_FILE}.{}.tmp", std::process::id()));
        {
            let mut file = fs::File::create(&tmp).map_err(|err| {
                Error::new(
                    ErrorKind::Io,
                    format!("无法写入临时状态文件 {}: {err}", tmp.display()),
                )
            })?;
            if let Err(err) = file.write_all(text.as_bytes()) {
                let _ = fs::remove_file(&tmp);
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("写入 {} 失败: {err}", tmp.display()),
                ));
            }
            file.flush().ok();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644));
        }
        fs::rename(&tmp, &self.path).map_err(|err| {
            let _ = fs::remove_file(&tmp);
            Error::new(
                ErrorKind::Io,
                format!("替换状态文件 {} 失败: {err}", self.path.display()),
            )
        })?;
        Ok(())
    }

    pub fn add(&mut self, record: MountRecord) {
        self.records.insert(record.device.clone(), record);
    }

    pub fn remove(&mut self, device: &str) -> Option<MountRecord> {
        if let Some(record) = self.records.remove(device) {
            return Some(record);
        }
        let wanted = basename(device);
        if wanted.is_empty() {
            return None;
        }
        let key = self
            .records
            .iter()
            .find(|(key, record)| basename(key) == wanted || basename(&record.device) == wanted)
            .map(|(key, _)| key.clone())?;
        self.records.remove(&key)
    }

    pub fn get(&self, device: &str) -> Option<&MountRecord> {
        self.records.get(device)
    }

    pub fn find_by_target(&self, target: &str) -> Option<&MountRecord> {
        let wanted = normalize(target);
        self.records
            .values()
            .find(|record| record.target == target)
            .or_else(|| {
                self.records
                    .values()
                    .find(|record| normalize(&record.target) == wanted)
            })
    }

    pub fn records(&self) -> impl Iterator<Item = &MountRecord> {
        self.records.values()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn directory(&self) -> PathBuf {
        match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        }
    }
}

/// 构造并加载状态（忽略读取错误）。
pub fn read_state(config: &Config) -> State {
    let mut state = State::new(config);
    if let Err(err) = state.load() {
        logging::warn(format!("读取状态失败: {err}"));
    }
    state
}

fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

fn normalize(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn none_if_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 把一条记录编码成 TAB 分隔的字段（不含行首的 `mount\t`）。
fn encode_mount_line(record: &MountRecord) -> String {
    let fields = [
        record.device.clone(),
        record.target.clone(),
        record.fstype.clone(),
        record.driver.clone(),
        record.uuid.clone().unwrap_or_default(),
        record.label.clone().unwrap_or_default(),
        record
            .owner_uid
            .map(|uid| uid.to_string())
            .unwrap_or_default(),
        record.reason.clone(),
        record.mounted_at.to_string(),
        record.options.join(","),
    ];
    fields
        .iter()
        .map(|field| encode_field(field))
        .collect::<Vec<String>>()
        .join("\t")
}

fn parse_mount_line(rest: &str) -> Option<MountRecord> {
    let fields: Vec<String> = rest.split('\t').map(decode_field).collect();
    if fields.len() < 10 {
        return None;
    }
    let owner_uid = match fields[6].trim() {
        "" => None,
        value => value.parse::<u32>().ok(),
    };
    let mounted_at = fields[8].trim().parse::<u64>().unwrap_or(0);
    let options = fields[9]
        .split(',')
        .map(str::trim)
        .filter(|option| !option.is_empty())
        .map(str::to_string)
        .collect::<Vec<String>>();
    Some(MountRecord {
        device: fields[0].clone(),
        target: fields[1].clone(),
        fstype: fields[2].clone(),
        driver: fields[3].clone(),
        uuid: none_if_empty(&fields[4]),
        label: none_if_empty(&fields[5]),
        owner_uid,
        reason: fields[7].clone(),
        mounted_at,
        options,
    })
}

fn encode_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            other => out.push(other),
        }
    }
    out
}

fn decode_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let mut digits = String::new();
        for _ in 0..2 {
            match chars.peek() {
                Some(next) if next.is_ascii_hexdigit() => {
                    digits.push(*next);
                    chars.next();
                }
                _ => break,
            }
        }
        if digits.len() == 2 {
            if let Ok(code) = u8::from_str_radix(&digits, 16) {
                if code.is_ascii() {
                    out.push(code as char);
                    continue;
                }
            }
        }
        out.push('%');
        out.push_str(&digits);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MountRecord;
    use std::path::Path;

    fn config_with(dir: &Path) -> Config {
        let mut config = Config::default();
        config.state_dir = dir.to_path_buf();
        config
    }

    fn record(device: &str, target: &str, label: Option<&str>) -> MountRecord {
        MountRecord {
            device: device.to_string(),
            target: target.to_string(),
            fstype: "exfat".to_string(),
            driver: "exfat".to_string(),
            uuid: Some("1234-ABCD".to_string()),
            label: label.map(str::to_string),
            owner_uid: Some(1000),
            reason: "hotplug".to_string(),
            mounted_at: 1_700_000_000,
            options: vec!["nosuid".to_string(), "nodev".to_string(), "uid=1000".to_string()],
        }
    }

    #[test]
    fn encodes_and_decodes_special_fields() {
        let raw = "a%b\tc\nd\re";
        let encoded = encode_field(raw);
        assert_eq!(encoded, "a%25b%09c%0Ad%0De");
        assert_eq!(decode_field(&encoded), raw);
        // 非合法转义保持原样
        assert_eq!(decode_field("100%zz"), "100%zz");
        assert_eq!(decode_field("50%"), "50%");
    }

    #[test]
    fn roundtrips_through_disk_with_tricky_label() {
        let dir = crate::testutil::TempDir::new("state-roundtrip");
        let config = config_with(dir.path());
        let mut state = State::new(&config);
        state.pid = 4321;
        state.started_at = 1_600_000_000;
        state.dry_run = true;
        state.backend = "udev".to_string();
        state.prepare().unwrap();

        let mut record = record("/dev/sdb1", "/media/alice/USB Disk", Some("USB\tDisk\n2"));
        record.options.push("label=%x".to_string());
        state.add(record.clone());
        state.save().unwrap();
        assert!(state.path.is_file());
        assert_eq!(state.path, dir.join("state.tsv"));

        let text = fs::read_to_string(&state.path).unwrap();
        assert!(text.starts_with(STATE_HEADER));
        assert_eq!(text.lines().filter(|line| line.starts_with("mount\t")).count(), 1);
        assert!(
            text.contains("USB%09Disk%0A2"),
            "卷标中的 TAB / 换行未被转义: {text}"
        );

        let mut loaded = State::new(&config);
        loaded.load().unwrap();
        assert_eq!(loaded.pid, 4321);
        assert_eq!(loaded.started_at, 1_600_000_000);
        assert!(loaded.dry_run);
        assert_eq!(loaded.backend, "udev");
        assert_eq!(loaded.get("/dev/sdb1"), Some(&record));
        assert_eq!(loaded.len(), 1);
        assert!(!loaded.is_empty());
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = crate::testutil::TempDir::new("state-missing");
        let config = config_with(&dir.join("never-created"));
        let mut state = State::new(&config);
        state.load().unwrap();
        assert!(state.is_empty());

        let read = read_state(&config);
        assert!(read.is_empty());
        assert_eq!(read.path, dir.join("never-created").join("state.tsv"));
    }

    #[test]
    fn skips_corrupt_lines_and_parses_meta() {
        let dir = crate::testutil::TempDir::new("state-corrupt");
        let path = dir.write(
            "state.tsv",
            concat!(
                "# automountd state v1\n",
                "pid=99\n",
                "started_at=123\n",
                "dry_run=yes\n",
                "backend=poll\n",
                "mount\t/dev/sdb1\t/media/a\texfat\texfat\tUUID-1\t卷标\t1000\thotplug\t1700000000\tnosuid,nodev\n",
                "mount\t坏行\n",
                "未知的行\n",
            ),
        );
        let mut state = State::new(&config_with(dir.path()));
        state.load().unwrap();
        assert_eq!(path, dir.join("state.tsv"));
        assert_eq!(state.pid, 99);
        assert_eq!(state.started_at, 123);
        assert!(state.dry_run);
        assert_eq!(state.backend, "poll");
        assert_eq!(state.len(), 1);
        let found = state.find_by_target("/media/a/").unwrap();
        assert_eq!(found.label.as_deref(), Some("卷标"));
        assert_eq!(found.options, vec!["nosuid", "nodev"]);
        assert_eq!(found.owner_uid, Some(1000));
    }

    #[test]
    fn removes_by_exact_path_or_basename() {
        let dir = crate::testutil::TempDir::new("state-remove");
        let config = config_with(dir.path());
        let mut state = State::new(&config);
        state.add(record("/dev/sdb1", "/media/a", None));
        state.add(record("/dev/sdc1", "/media/b", None));
        assert_eq!(state.len(), 2);

        let removed = state.remove("/dev/sdb1").unwrap();
        assert_eq!(removed.target, "/media/a");
        assert_eq!(state.len(), 1);

        let removed = state.remove("sdc1").unwrap();
        assert_eq!(removed.device, "/dev/sdc1");
        assert!(state.remove("不存在").is_none());
        assert!(state.is_empty());
        assert!(state.find_by_target("/media/a").is_none());
    }

    #[test]
    fn saves_empty_state() {
        let dir = crate::testutil::TempDir::new("state-empty");
        let config = config_with(&dir.join("nested").join("dir"));
        let state = State::new(&config);
        state.save().unwrap();
        assert!(config.state_dir.join("state.tsv").is_file());
        let text = fs::read_to_string(config.state_dir.join("state.tsv")).unwrap();
        assert!(text.contains("pid="));
        assert!(!text.contains("mount\t"));
    }
}
