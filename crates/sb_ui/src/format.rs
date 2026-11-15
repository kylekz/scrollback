//! How the views write numbers, dates, sizes and paths (DESIGN §4.1, §4.4, §4.6). Times are Unix
//! milliseconds, as the catalogue keeps them, and are shown in the machine's time zone.

use chrono::{DateTime, Datelike as _, Local, TimeZone as _};

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Now, in Unix milliseconds.
pub fn now() -> i64 {
    Local::now().timestamp_millis()
}

/// `2318` as `2,318`.
pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `1 session`, `2,318 sessions`.
pub fn plural(value: usize, one: &str) -> String {
    format!(
        "{} {one}{}",
        count(value as u64),
        if value == 1 { "" } else { "s" }
    )
}

/// The local time of `ms`; `None` for a time no calendar has.
fn local(ms: i64) -> Option<DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

/// The calendar month `ms` falls in, as `(year, month)` with January as 1.
pub fn month_of(ms: i64) -> Option<(i32, u32)> {
    local(ms).map(|time| (time.year(), time.month()))
}

/// `November 2026`.
pub fn month_label((year, month): (i32, u32)) -> String {
    format!("{} {year}", MONTHS[(month as usize + 11) % 12])
}

/// `12 Nov`, or `12 Dec 2025` for a day of another year than `now`'s.
pub fn day(ms: i64, now: i64) -> String {
    let Some(time) = local(ms) else {
        return String::new();
    };
    let month = &MONTHS[time.month0() as usize][..3];
    if local(now).is_some_and(|now| now.year() == time.year()) {
        format!("{} {month}", time.day())
    } else {
        format!("{} {month} {}", time.day(), time.year())
    }
}

/// `09:02`.
pub fn clock(ms: i64) -> String {
    local(ms).map_or_else(String::new, |time| time.format("%H:%M").to_string())
}

/// `Nov 2025 – Nov 2026`, or `Nov 2026` when both ends are in one month.
pub fn span(from: i64, to: i64) -> String {
    let short = |ms| {
        month_of(ms).map(|(year, month)| format!("{} {year}", &MONTHS[month as usize - 1][..3]))
    };
    match (short(from), short(to)) {
        (Some(from), Some(to)) if from != to => format!("{from} – {to}"),
        (Some(month), _) | (None, Some(month)) => month,
        (None, None) => "—".to_string(),
    }
}

/// How long ago `ms` was, seen from `now`: `just now`, `2 min ago`, `3 hours ago`, `12 days
/// ago`.
pub fn ago(ms: i64, now: i64) -> String {
    let minutes = (now - ms).max(0) / 60_000;
    let (value, unit) = match minutes {
        0 => return "just now".to_string(),
        1..60 => return format!("{minutes} min ago"),
        60..1440 => (minutes / 60, "hour"),
        _ => (minutes / 1440, "day"),
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

/// How long `seconds` is, roughly: `about 20 min`, `about 2 hours`, `under a minute`.
pub fn about(seconds: u64) -> String {
    match seconds {
        0..60 => "under a minute".to_string(),
        60..5400 => format!("about {} min", (seconds + 30) / 60),
        _ => format!("about {} hours", (seconds + 1800) / 3600),
    }
}

/// `done` of `total` bytes in the unit that suits the total: `41 / 96 MB`, `1.1 / 1.4 GB`.
pub fn bytes_of(done: u64, total: u64, separator: &str) -> String {
    const UNITS: [(&str, u64); 3] = [("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    let (unit, size) = UNITS
        .into_iter()
        .find(|(_, size)| total >= *size)
        .unwrap_or(("KB", 1 << 10));
    let amount = |bytes: u64| {
        let value = bytes as f64 / size as f64;
        if unit == "GB" {
            format!("{value:.1}")
        } else {
            format!("{value:.0}")
        }
    };
    format!("{}{separator}{} {unit}", amount(done), amount(total))
}

/// A path as its machine writes it. A provider joins a location's home and its own folders
/// with `/`, which on a Windows drive reads `C:\Users\user/.agent/projects`.
pub fn path(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

/// The last component of a path, whichever separator it uses: `/home/user/code/advent-2025`
/// is `advent-2025`.
pub fn last_component(path: &str) -> Option<&str> {
    path.rsplit(['/', '\\']).find(|part| !part.is_empty())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// Noon, local time, on the day given.
    fn at(year: i32, month: u32, day: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, 12, 0, 0)
            .single()
            .expect("noon exists in every zone")
            .timestamp_millis()
    }

    #[test]
    fn counts_group_their_thousands() {
        assert_eq!(count(0), "0");
        assert_eq!(count(488), "488");
        assert_eq!(count(2318), "2,318");
        assert_eq!(count(52_914), "52,914");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(plural(1, "session"), "1 session");
        assert_eq!(plural(0, "session"), "0 sessions");
        assert_eq!(plural(1414, "session"), "1,414 sessions");
    }

    #[test]
    fn days_and_months_read_as_the_boards_write_them() {
        let now = at(2026, 11, 16);
        assert_eq!(day(at(2026, 11, 12), now), "12 Nov");
        assert_eq!(day(at(2025, 12, 12), now), "12 Dec 2025");
        assert_eq!(month_of(at(2026, 11, 12)), Some((2026, 11)));
        assert_eq!(month_label((2026, 11)), "November 2026");
        assert_eq!(month_label((2026, 1)), "January 2026");
        assert_eq!(
            span(at(2025, 11, 3), at(2026, 11, 12)),
            "Nov 2025 – Nov 2026"
        );
        assert_eq!(span(at(2026, 11, 3), at(2026, 11, 12)), "Nov 2026");
    }

    #[test]
    fn elapsed_time_is_rounded_to_what_matters() {
        let minute = 60_000;
        assert_eq!(ago(1_000, 30_000), "just now");
        assert_eq!(ago(0, 2 * minute), "2 min ago");
        assert_eq!(ago(0, 60 * minute), "1 hour ago");
        assert_eq!(ago(0, 12 * 24 * 60 * minute), "12 days ago");
        // A clock set back never reads as the future.
        assert_eq!(ago(5 * minute, 0), "just now");
        assert_eq!(about(20), "under a minute");
        assert_eq!(about(20 * 60), "about 20 min");
        assert_eq!(about(2 * 3600), "about 2 hours");
    }

    #[test]
    fn sizes_share_the_totals_unit() {
        assert_eq!(bytes_of(41 << 20, 96 << 20, " of "), "41 of 96 MB");
        assert_eq!(
            bytes_of(1_181_116_006, 1_503_238_553, " / "),
            "1.1 / 1.4 GB"
        );
        assert_eq!(bytes_of(0, 300, " / "), "0 / 0 KB");
    }

    #[test]
    fn paths_keep_their_machines_separator() {
        assert_eq!(
            path("C:\\Users\\user/.agent/projects"),
            "C:\\Users\\user\\.agent\\projects"
        );
        assert_eq!(
            path("/home/user/.agent/projects"),
            "/home/user/.agent/projects"
        );
        assert_eq!(
            last_component("/home/user/code/advent-2025"),
            Some("advent-2025")
        );
        assert_eq!(last_component("D:\\Projects\\fc\\"), Some("fc"));
        assert_eq!(last_component("/"), None);
    }
}
