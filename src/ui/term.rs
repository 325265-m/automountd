//! 终端输出辅助：颜色、表格、大小与时间格式化。

use crate::naming::display_width;
use crate::platform::now_epoch;

#[derive(Debug, Clone, Copy)]
pub struct Colors {
    pub enabled: bool,
}

impl Colors {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub fn auto() -> Self {
        Self {
            enabled: crate::platform::stdout_is_tty(),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn red(&self, text: &str) -> String {
        self.paint("31", text)
    }

    pub fn green(&self, text: &str) -> String {
        self.paint("32", text)
    }

    pub fn yellow(&self, text: &str) -> String {
        self.paint("33", text)
    }

    pub fn blue(&self, text: &str) -> String {
        self.paint("34", text)
    }

    pub fn cyan(&self, text: &str) -> String {
        self.paint("36", text)
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }
}

pub fn pad(text: &str, width: usize) -> String {
    let current = display_width(text);
    let mut padded = text.to_string();
    if current < width {
        padded.push_str(&" ".repeat(width - current));
    }
    padded
}

/// 生成对齐的纯文本表格（按显示宽度对齐，支持中文）。
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(widths.len()) {
            widths[index] = widths[index].max(display_width(cell));
        }
    }

    let mut output = String::new();
    let header_cells: Vec<String> = headers.iter().map(|h| (*h).to_string()).collect();
    output.push_str(&render_row(&header_cells, &widths));
    output.push('\n');
    for row in rows {
        output.push_str(&render_row(row, &widths));
        output.push('\n');
    }
    output
}

fn render_row(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::new();
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        if index + 1 == widths.len() {
            line.push_str(cell);
        } else {
            line.push_str(&pad(cell, *width));
            line.push_str("  ");
        }
    }
    line.trim_end().to_string()
}

/// 把字节数格式化为 1.5 GiB 这样的形式。
pub fn format_size(size: u64) -> String {
    if size == 0 {
        return "-".to_string();
    }
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

/// Unix 秒 -> `YYYY-MM-DD HH:MM:SS`（UTC）。
pub fn format_timestamp(epoch: u64) -> String {
    let secs = epoch as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 当前时间字符串，供日志使用。
pub fn now_string() -> String {
    let text = format_timestamp(now_epoch());
    text.trim_end_matches(" UTC").to_string()
}

pub fn format_time_of_day(epoch: u64) -> String {
    let rem = (epoch as i64).rem_euclid(86_400);
    format!("{:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// 相对时间（中文），例如 “3 分钟前”。
pub fn relative_time(epoch: u64) -> String {
    relative_time_at(epoch, now_epoch())
}

pub fn relative_time_at(epoch: u64, now: u64) -> String {
    if epoch == 0 {
        return "-".to_string();
    }
    if now < epoch {
        return format_timestamp(epoch);
    }
    let delta = now - epoch;
    match delta {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", delta / 60),
        3600..=86_399 => format!("{} 小时前", delta / 3600),
        86_400..=2_591_999 => format!("{} 天前", delta / 86_400),
        _ => format_timestamp(epoch),
    }
}

/// 按显示宽度截断，超出部分用省略号。
pub fn truncate(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let mut result = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let char_width = display_width(&ch.to_string());
        if width + char_width > max_width - 1 {
            break;
        }
        result.push(ch);
        width += char_width;
    }
    result.push('…');
    result
}

/// Howard Hinnant 的 civil_from_days 算法（公历）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "-");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn formats_epoch() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }

    #[test]
    fn relative_times() {
        let now = 1_700_000_000;
        assert_eq!(relative_time_at(now - 10, now), "刚刚");
        assert_eq!(relative_time_at(now - 120, now), "2 分钟前");
        assert_eq!(relative_time_at(now - 7200, now), "2 小时前");
        assert_eq!(relative_time_at(now - 172_800, now), "2 天前");
    }

    #[test]
    fn aligns_cjk_width() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("挂载"), 4);
        assert_eq!(pad("挂载", 6), "挂载  ");
    }

    #[test]
    fn truncates_by_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
    }
}
