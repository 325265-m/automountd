//! 极简运行时语言选择（中文 / English）。
//!
//! 优先级：`--lang` > `AUTOMOUNTD_LANG` > `LC_ALL` > `LANG` > 中文。
//! 只影响面向用户的文案，不改变日志字段、协议或配置文件格式。

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

static CURRENT: AtomicU8 = AtomicU8::new(Lang::Zh as u8);

impl Lang {
    fn as_u8(self) -> u8 {
        match self {
            Lang::Zh => 0,
            Lang::En => 1,
        }
    }
}

/// 初始化语言。`explicit` 来自 `--lang`；传 `"auto"` 或 `None` 时使用环境变量。
pub fn init(explicit: Option<&str>) {
    let value = match explicit.map(str::trim) {
        Some(value) if !value.is_empty() && !value.eq_ignore_ascii_case("auto") => {
            value.to_string()
        }
        _ => std::env::var("AUTOMOUNTD_LANG")
            .or_else(|_| std::env::var("LC_ALL"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default(),
    };
    set(&value);
}

/// 直接设置语言标签（`en`、`en_US.UTF-8`、`zh_CN` 等）。
pub fn set(tag: &str) {
    let lang = if is_english_tag(tag) {
        Lang::En
    } else {
        Lang::Zh
    };
    CURRENT.store(lang.as_u8(), Ordering::SeqCst);
}

pub fn current() -> Lang {
    if CURRENT.load(Ordering::SeqCst) == Lang::En.as_u8() {
        Lang::En
    } else {
        Lang::Zh
    }
}

pub fn is_en() -> bool {
    current() == Lang::En
}

/// 根据当前语言从两条静态文案中选择一条。
pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    if is_en() {
        en
    } else {
        zh
    }
}

pub fn is_english_tag(tag: &str) -> bool {
    let tag = tag.trim().to_ascii_lowercase();
    tag == "en" || tag.starts_with("en_") || tag.starts_with("en-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_english_tags() {
        assert!(is_english_tag("en"));
        assert!(is_english_tag("en_US.UTF-8"));
        assert!(is_english_tag("en-GB"));
        assert!(!is_english_tag("zh_CN.UTF-8"));
        assert!(!is_english_tag("C.UTF-8"));
        assert!(!is_english_tag(""));
    }

    #[test]
    fn tr_selects_by_current_language() {
        set("en_US.UTF-8");
        assert_eq!(tr("中文", "English"), "English");
        set("zh_CN.UTF-8");
        assert_eq!(tr("中文", "English"), "中文");
    }
}
