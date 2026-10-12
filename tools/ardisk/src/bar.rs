//! Usage bars for the flat directory list: `████░░░░░░  42%`.
//!
//! Each directory's share is relative to the scanned root, which is always
//! 100%. The bar is drawn with full blocks on a light-shade track, rounded
//! to the nearest cell; the percentage next to it gives the exact figure.

use crate::tree::percent;
use std::path::PathBuf;

/// Cells in a bar.
pub const BAR_WIDTH: usize = 10;

/// A bar of `width` cells for `value` out of `total`.
pub fn bar(value: u64, total: u64, width: usize) -> String {
    let filled = if total == 0 {
        0
    } else {
        // Round to the nearest cell, never past the end.
        ((value as f64 / total as f64) * width as f64).round() as usize
    }
    .min(width);
    let mut out = String::with_capacity(width * 3);
    out.extend(std::iter::repeat_n('█', filled));
    out.extend(std::iter::repeat_n('░', width - filled));
    out
}

/// Renders the flat directory list with a bar and percentage column:
///
/// ```text
///   6.68 GB  ██████████ 100%  /usr
///   3.10 GB  █████░░░░░  46%  /usr/lib
/// ```
///
/// `plain` formats a value; `paint` may colour the padded text afterwards so
/// colour codes never affect alignment. The value column is at least 10
/// characters wide, like the list without bars.
pub fn render_list_with_bars(
    entries: &[(PathBuf, u64)],
    total: u64,
    plain: impl Fn(u64) -> String,
    paint: impl Fn(u64, String) -> String,
) -> String {
    let values: Vec<String> = entries.iter().map(|(_, v)| plain(*v)).collect();
    let width = values
        .iter()
        .map(|v| v.chars().count())
        .max()
        .unwrap_or(0)
        .max(10);
    let mut out = String::new();
    for ((path, value), text) in entries.iter().zip(values) {
        out.push_str(&format!(
            "{}  {} {:>4}  {}\n",
            paint(*value, format!("{text:>width$}")),
            bar(*value, total, BAR_WIDTH),
            percent(*value, total),
            path.display()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_fills_proportionally_and_rounds() {
        assert_eq!(bar(0, 100, 10), "░░░░░░░░░░");
        assert_eq!(bar(42, 100, 10), "████░░░░░░");
        assert_eq!(bar(45, 100, 10), "█████░░░░░");
        assert_eq!(bar(100, 100, 10), "██████████");
        assert_eq!(bar(4, 100, 10), "░░░░░░░░░░");
        assert_eq!(bar(1, 3, 6), "██░░░░");
    }

    #[test]
    fn bar_handles_zero_total_and_overflow() {
        assert_eq!(bar(5, 0, 4), "░░░░");
        assert_eq!(bar(200, 100, 4), "████");
        assert_eq!(bar(u64::MAX, u64::MAX, 3), "███");
        assert_eq!(bar(1, 1, 0), "");
    }

    #[test]
    fn list_has_aligned_value_bar_and_percent_columns() {
        let e = vec![
            (PathBuf::from("/r"), 1000),
            (PathBuf::from("/r/a"), 420),
            (PathBuf::from("/r/b"), 5),
        ];
        let out = render_list_with_bars(&e, 1000, |v| v.to_string(), |_, t| t);
        assert_eq!(
            out,
            "      1000  ██████████ 100%  /r\n       420  ████░░░░░░  42%  /r/a\n         5  ░░░░░░░░░░   0%  /r/b\n"
        );
    }

    #[test]
    fn paint_gets_padded_text() {
        let e = vec![(PathBuf::from("/r"), 7)];
        let out = render_list_with_bars(&e, 7, |v| v.to_string(), |_, t| format!("<{t}>"));
        assert!(out.starts_with("<         7>  "), "{out:?}");
    }

    #[test]
    fn long_values_widen_the_column() {
        let e = vec![(PathBuf::from("/r"), 1)];
        let out = render_list_with_bars(&e, 1, |_| "123456789012".into(), |_, t| t);
        assert!(out.starts_with("123456789012  "), "{out:?}");
    }
}
