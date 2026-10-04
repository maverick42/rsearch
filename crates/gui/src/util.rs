//! Small pure helpers for the GUI: text-list parsing and display
//! formatting. No GUI-toolkit dependency, so everything here is
//! unit-testable.

use std::time::Duration;

/// Splits a comma/semicolon/newline separated text into trimmed,
/// non-empty items.
pub fn parse_list(text: &str) -> Vec<String> {
    text.split([',', ';', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Joins a stored list into the comma-separated editing form.
pub fn join_list(items: &[String]) -> String {
    items.join(", ")
}

/// The max-size text as a whole number of MiB, or `None` when it is
/// not a positive integer. Shared by the project editor and the
/// preferences screen — same field semantics, same inline error.
pub fn parse_max_size_mib(text: &str) -> Option<u64> {
    match text.trim().parse::<u64>() {
        Ok(0) | Err(_) => None,
        Ok(mib) => Some(mib),
    }
}

/// Formats a unix timestamp (seconds) as `YYYY-MM-DD HH:MM UTC`.
/// No timezone database is involved; the UTC suffix keeps the
/// rendering honest.
pub fn format_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        tod / 3600,
        (tod % 3600) / 60
    )
}

/// Days since epoch → (year, month, day). Howard Hinnant's
/// `civil_from_days` algorithm, exact for any date the i64 range covers.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Visually truncates a single-line title to at most `max` characters,
/// the last one replaced by "…" — display only, the source string is
/// never modified.
pub fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Compact byte size: `512 B`, `12.3 MiB`, `3.0 GiB`.
pub fn format_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.1} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

/// Compact duration: `12.3 s` under a minute, `2 m 05 s` beyond.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{:.1} s", d.as_secs_f64())
    } else {
        format!("{} m {:02} s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_splits_and_trims() {
        assert_eq!(
            parse_list(" target, build ;\n.git ,\n\n dist "),
            vec!["target", "build", ".git", "dist"]
        );
        assert!(parse_list("  , ; \n").is_empty());
    }

    #[test]
    fn join_list_round_trip() {
        let items = vec!["a".to_string(), "b".to_string()];
        assert_eq!(parse_list(&join_list(&items)), items);
    }

    #[test]
    fn parse_max_size_mib_rejects_non_numeric_and_zero() {
        assert_eq!(parse_max_size_mib("abc"), None);
        assert_eq!(parse_max_size_mib("0"), None);
        assert_eq!(parse_max_size_mib(""), None);
        assert_eq!(parse_max_size_mib(" 12 "), Some(12));
    }

    #[test]
    fn format_unix_known_dates() {
        // 2024-10-03 00:00:00 UTC = 1727913600.
        assert_eq!(format_unix(1_727_913_600), "2024-10-03 00:00 UTC");
        // Epoch and a pre-epoch date.
        assert_eq!(format_unix(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_unix(-86_400), "1969-12-31 00:00 UTC");
        // Leap day: 2024-02-29 12:34 UTC = 1709210040.
        assert_eq!(format_unix(1_709_210_040), "2024-02-29 12:34 UTC");
    }

    #[test]
    fn ellipsize_short_text_is_untouched() {
        assert_eq!(ellipsize("hello", 10), "hello");
        assert_eq!(ellipsize("1234567890", 10), "1234567890");
    }

    #[test]
    fn ellipsize_long_text_is_cut_on_chars() {
        let long = "a very long search query that overflows the tab";
        let out = ellipsize(long, 24);
        assert_eq!(out.chars().count(), 24);
        assert!(out.ends_with('…'));
        // Multi-byte characters count once and are never split.
        let out = ellipsize("éééééééé", 5);
        assert_eq!(out, "éééé…");
    }

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2_048), "2.0 KiB");
        assert_eq!(format_bytes(16 * 1024 * 1024), "16.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn format_duration_human() {
        assert_eq!(format_duration(Duration::from_millis(12_345)), "12.3 s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2 m 05 s");
    }
}
