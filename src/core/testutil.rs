//! 测试辅助（非公开 API）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 自动清理的临时目录。
///
/// 默认放系统临时目录；如果运行环境禁止子进程写系统 temp（例如受限沙箱），
/// 自动退回到项目内的 `target/test-tmp`，保证测试可重复运行。
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let unique = format!(
            "{prefix}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let system_candidate = std::env::temp_dir().join(&unique);
        let workspace_candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(&unique);

        let mut last_error = None;
        for path in [system_candidate, workspace_candidate] {
            match std::fs::create_dir_all(&path) {
                Ok(()) => return Self { path },
                Err(err) => last_error = Some((path, err)),
            }
        }

        let (path, err) = last_error.expect("至少应尝试一个临时目录");
        panic!(
            "创建临时目录失败 {}（系统 temp={:?}）: {err}",
            path.display(),
            std::env::temp_dir()
        );
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.join(name);
        std::fs::write(&path, text).expect("写入临时文件失败");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
