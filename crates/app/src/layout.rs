// SPDX-License-Identifier: EUPL-1.1
//! 终端自适应分栏布局引擎（CJK 宽字符与 ANSI 色彩感知，绝不遮挡/换行错位）。
//!
//! 规则：
//! - 查询 Windows 控制台真实可视窗口宽高（`GetConsoleScreenBufferInfo` 的 `srWindow`）；
//! - 宽度 >= 96 列（如 Windows Terminal 默认 120 列）时启用左右分栏：
//!   左侧显示主界面/状态与实时日志，右侧固定 36 列显示精简操作对照与指示灯速查，
//!   左栏自动按列宽截断并闭合 ANSI 颜色，绝不溢出遮挡右侧；
//! - 宽度 < 96 列（如 80 列窄窗口）时自动退化为上下堆叠，确保所有内容完整可见。

use crate::util::ansi;

/// 右侧精简操作指引栏固定列宽
pub(crate) const RIGHT_PANEL_WIDTH: usize = 36;
/// 启用左右分栏所需的最小左栏宽度
const MIN_LEFT_WIDTH: usize = 56;
/// 分栏分隔符（3 列）
const DIVIDER: &str = " | ";

/// 获取当前 Windows 控制台可视窗口尺寸 `(列数, 行数)`；非控制台或查询失败时回退 `(80, 24)`。
pub(crate) fn terminal_size() -> (usize, usize) {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return (80, 24);
    }
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
        return (80, 24);
    }
    let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(40) as usize;
    let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(16) as usize;
    (cols, rows)
}

/// 判断给定终端宽度是否足以左右分栏而不遮挡。
pub(crate) fn supports_side_by_side(term_width: usize) -> bool {
    term_width > MIN_LEFT_WIDTH + DIVIDER.len() + RIGHT_PANEL_WIDTH
}

/// 计算左栏可用列宽（留出 1 列右边界防 Windows 控制台自动换行）。
pub(crate) fn left_column_width(term_width: usize) -> usize {
    if supports_side_by_side(term_width) {
        term_width - DIVIDER.len() - RIGHT_PANEL_WIDTH - 1
    } else {
        term_width.saturating_sub(1).max(40)
    }
}

/// 将左栏主内容与右栏精简指引合成为最终屏幕文本。
/// - 宽终端：左右并排，中间以 ` | ` 分隔，每行总宽严格 <= `term_width - 1`；
/// - 窄终端：左栏结束后在下方以横线追加精简指引，绝不挤压/遮挡左栏。
pub(crate) fn compose_two_columns(left: &[String], right: &[String], term_width: usize) -> String {
    let mut out = String::new();
    if supports_side_by_side(term_width) {
        let left_w = left_column_width(term_width);
        let rows = left.len().max(right.len());
        let dim_div = format!("{}{}{}", ansi("90"), DIVIDER, ansi("0"));
        for i in 0..rows {
            let l_raw = left.get(i).map(String::as_str).unwrap_or("");
            let r_raw = right.get(i).map(String::as_str).unwrap_or("");
            let l_fit = fit_to_width(l_raw, left_w);
            let r_fit = truncate_to_width(r_raw, RIGHT_PANEL_WIDTH);
            out.push_str(&l_fit);
            out.push_str(&dim_div);
            out.push_str(&r_fit);
            out.push('\n');
        }
    } else {
        let max_w = left_column_width(term_width);
        for l in left {
            out.push_str(&truncate_to_width(l, max_w));
            out.push('\n');
        }
        if !right.is_empty() {
            out.push_str(&"-".repeat(max_w.min(60)));
            out.push('\n');
            for r in right {
                out.push_str(&truncate_to_width(r, max_w));
                out.push('\n');
            }
        }
    }
    out
}

/// 单个字符在 Windows 终端中的显示列宽（CJK 与全角标点为 2，其余为 1）。
pub(crate) fn char_width(c: char) -> usize {
    let u = c as u32;
    if c.is_control() {
        return 0;
    }
    let wide = matches!(
        u,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3040..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF01..=0xFF60
            | 0xFFE0..=0xFFE6
    );
    if wide { 2 } else { 1 }
}

/// 计算包含 ANSI CSI 转义序列的字符串实际占用的终端列宽。
pub(crate) fn visible_width(s: &str) -> usize {
    let mut w = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for nc in chars.by_ref() {
                if ('@'..='~').contains(&nc) {
                    break;
                }
            }
            continue;
        }
        w += char_width(c);
    }
    w
}

/// 将字符串按终端列宽安全截断（不破坏 UTF-8 与 ANSI 转义序列，必要时自动闭合 `\x1b[0m`）。
pub(crate) fn truncate_to_width(s: &str, max_cols: usize) -> String {
    if visible_width(s) <= max_cols {
        return s.to_string();
    }
    let limit = max_cols.saturating_sub(3);
    let mut out = String::with_capacity(s.len());
    let mut used = 0usize;
    let mut ansi_open = false;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            out.push(c);
            out.push(chars.next().unwrap());
            let mut seq = String::new();
            for nc in chars.by_ref() {
                out.push(nc);
                seq.push(nc);
                if ('@'..='~').contains(&nc) {
                    break;
                }
            }
            ansi_open = seq != "0m" && seq != "m";
            continue;
        }
        let cw = char_width(c);
        if used + cw > limit {
            break;
        }
        out.push(c);
        used += cw;
    }

    if max_cols >= 3 {
        out.push_str("...");
    }
    if ansi_open {
        out.push_str("\x1b[0m");
    }
    out
}

/// 将字符串精确适配为 `target_cols` 列：超宽截断，不足补空格，并确保 ANSI 颜色不泄漏到右栏。
pub(crate) fn fit_to_width(s: &str, target_cols: usize) -> String {
    let mut fitted = truncate_to_width(s, target_cols);
    let w = visible_width(&fitted);
    if has_unclosed_ansi(&fitted) {
        fitted.push_str("\x1b[0m");
    }
    if w < target_cols {
        fitted.push_str(&" ".repeat(target_cols - w));
    }
    fitted
}

fn has_unclosed_ansi(s: &str) -> bool {
    let mut open = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            let mut seq = String::new();
            for nc in chars.by_ref() {
                seq.push(nc);
                if ('@'..='~').contains(&nc) {
                    break;
                }
            }
            open = seq != "0m" && seq != "m";
        }
    }
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_counts_cjk_and_ignores_ansi() {
        assert_eq!(visible_width("abc"), 3);
        assert_eq!(visible_width("路由器"), 6);
        assert_eq!(visible_width("\x1b[32m[正常]\x1b[0m 测试"), 11);
    }

    #[test]
    fn fit_to_width_never_exceeds_target_and_closes_ansi() {
        let long = "\x1b[31m这是一段非常长非常长的中文告警信息用来测试截断边界\x1b[0m";
        let fitted = fit_to_width(long, 20);
        assert_eq!(visible_width(&fitted), 20);
        assert!(fitted.ends_with("\x1b[0m") || !has_unclosed_ansi(&fitted));
    }

    #[test]
    fn compose_two_columns_respects_terminal_width() {
        let left = vec!["左侧状态行".into(), "a".repeat(120)];
        let right = vec!["=== 操作指引 ===".into(), "1. 插路由器LAN口".into()];

        // 120 列宽终端：并排且每行严格不超过 119 列
        let wide = compose_two_columns(&left, &right, 120);
        assert!(wide.contains(" | "));
        for line in wide.lines() {
            assert!(
                visible_width(line) < 120,
                "line overflowed width 120: {} cols",
                visible_width(line)
            );
        }

        // 80 列窄终端：自动堆叠，不含分栏分隔符，且无行超宽
        let narrow = compose_two_columns(&left, &right, 80);
        assert!(!narrow.contains(" | "));
        for line in narrow.lines() {
            assert!(
                visible_width(line) < 80,
                "narrow line overflowed width 80: {} cols",
                visible_width(line)
            );
        }
    }
}
