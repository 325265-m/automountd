//! Minimal runtime language selection (Chinese / English).
//!
//! Precedence: `--lang` > `AUTOMOUNTD_LANG` > `LC_ALL` > `LANG` > English.
//! Only affects user-facing text; log fields, protocol, and config file format are unchanged.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

// English is the default; Chinese is opt-in via --lang zh or LANG=zh_CN.
static CURRENT: AtomicU8 = AtomicU8::new(1);

impl Lang {
    fn as_u8(self) -> u8 {
        match self {
            Lang::Zh => 0,
            Lang::En => 1,
        }
    }
}

/// Initializes the language. `explicit` comes from `--lang`; `"auto"` or `None` uses the environment.
pub fn init(explicit: Option<&str>) {
    let value = match explicit.map(str::trim) {
        Some(value) if !value.is_empty() && !value.eq_ignore_ascii_case("auto") => {
            value.to_string()
        }
        _ => std::env::var("AUTOMOUNTD_LANG")
            .or_else(|_| std::env::var("LC_ALL"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_else(|_| "en".to_string()),
    };
    let value = if value.trim().is_empty() {
        "en".to_string()
    } else {
        value
    };
    set(&value);
}

/// Sets the language tag directly (`en`, `en_US.UTF-8`, `zh_CN`, and so on).
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

/// Picks one of two static strings according to the current language.
pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    if is_en() {
        en
    } else {
        zh
    }
}

pub fn is_english_tag(tag: &str) -> bool {
    let tag = tag.trim().to_ascii_lowercase();
    tag == "en"
        || tag.starts_with("en_")
        || tag.starts_with("en-")
        || matches!(tag.as_str(), "c" | "c.utf-8" | "c.utf8" | "posix")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_english_tags() {
        assert!(is_english_tag("en"));
        assert!(is_english_tag("en_US.UTF-8"));
        assert!(is_english_tag("en-GB"));
        assert!(is_english_tag("C.UTF-8"));
        assert!(!is_english_tag("zh_CN.UTF-8"));
        assert!(!is_english_tag("fr_FR.UTF-8"));
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
