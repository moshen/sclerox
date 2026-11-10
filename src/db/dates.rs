//! The `--from`/`--to` range searches keep their records to.

use anyhow::{bail, Result};
use chrono::{Days, Months, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

/// How dates are stored, and how SQLite's `datetime()` writes them.
const STAMP: &str = "%Y-%m-%d %H:%M:%S";

/// A range of times, both ends included, that each record's date must fall
/// in, and whether that date is when the record was created rather than when
/// it last changed. The default is no range: every record, dated or not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dates {
    from: Option<NaiveDateTime>,
    to: Option<NaiveDateTime>,
    pub created: bool,
}

impl Dates {
    /// The range from `--from` and `--to`, each a date (`2026-06-08`, with
    /// an optional `HH:MM[:SS]` time) or an age counted back from today in
    /// days, weeks, months or years (`7d`, `2w`, `3m`, `1y`). A bound
    /// without a time takes in the whole day. Times are UTC, as stored.
    pub fn parse(from: Option<&str>, to: Option<&str>, created: bool) -> Result<Dates> {
        Self::parse_on(from, to, created, chrono::Utc::now().date_naive())
    }

    fn parse_on(
        from: Option<&str>,
        to: Option<&str>,
        created: bool,
        today: NaiveDate,
    ) -> Result<Dates> {
        let from = from
            .map(|s| bound(s, today, false).ok_or_else(|| unreadable("--from", s)))
            .transpose()?;
        let to = to
            .map(|s| bound(s, today, true).ok_or_else(|| unreadable("--to", s)))
            .transpose()?;
        if let (Some(from), Some(to)) = (from, to) {
            if from > to {
                bail!("--from ({from}) is after --to ({to})");
            }
        }
        Ok(Dates { from, to, created })
    }

    /// Whether this keeps every record, dated or not.
    pub fn is_any(&self) -> bool {
        self.from.is_none() && self.to.is_none()
    }

    /// `changed`, or `created` under `--created`: the column a table tests.
    pub fn pick<'a>(&self, changed: &'a str, created: &'a str) -> &'a str {
        if self.created {
            created
        } else {
            changed
        }
    }

    /// SQL keeping rows whose `column` is in range, with the bounds as
    /// parameters `?n` and `?n+1` (see [`Dates::values`]). `datetime()` reads
    /// a plain date as its midnight, and NULL, or a date it can't read, is
    /// out of any range.
    pub fn sql(&self, column: &str, n: usize) -> String {
        let m = n + 1;
        format!(
            "(?{n} IS NULL OR datetime({column}) >= ?{n}) \
             AND (?{m} IS NULL OR datetime({column}) <= ?{m})"
        )
    }

    /// The bounds [`Dates::sql`] takes, in order; NULL for an open end.
    pub fn values(&self) -> [Option<String>; 2] {
        [self.from, self.to].map(|b| b.map(|b| b.format(STAMP).to_string()))
    }

    /// Whether a record last changed at `changed` and created at `created`
    /// is in range.
    pub fn keeps(&self, changed: &str, created: &str) -> bool {
        self.contains(Some(self.pick(changed, created)))
    }

    /// Whether a record dated `stamp` is in range, as [`Dates::sql`] decides.
    pub fn contains(&self, stamp: Option<&str>) -> bool {
        if self.is_any() {
            return true;
        }
        let Some(at) = stamp.and_then(read_stamp) else {
            return false;
        };
        self.from.is_none_or(|from| at >= from) && self.to.is_none_or(|to| at <= to)
    }
}

fn unreadable(flag: &str, value: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{flag} '{value}': expected a date like 2026-06-08 or an age like 7d, 2w, 3m, 1y"
    )
}

/// The first moment of a `--from` bound, or the last of a `--to` one.
fn bound(s: &str, today: NaiveDate, end: bool) -> Option<NaiveDateTime> {
    let s = s.trim();
    let day_bound = |day: NaiveDate| {
        let time = if end {
            NaiveTime::from_hms_opt(23, 59, 59)?
        } else {
            NaiveTime::MIN
        };
        Some(day.and_time(time))
    };
    if let Some(day) = age(s, today) {
        return day_bound(day);
    }
    if let Ok(day) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return day_bound(day);
    }
    let s = s.replacen('T', " ", 1);
    if let Ok(at) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M") {
        // The whole minute.
        return Some(if end {
            at + chrono::Duration::seconds(59)
        } else {
            at
        });
    }
    NaiveDateTime::parse_from_str(&s, STAMP).ok()
}

/// The day `s` (`7d`, `2w`, `3m`, `1y`) before `today`.
fn age(s: &str, today: NaiveDate) -> Option<NaiveDate> {
    let unit = s.chars().last()?;
    let n: u32 = s[..s.len() - unit.len_utf8()].parse().ok()?;
    match unit.to_ascii_lowercase() {
        'd' => today.checked_sub_days(Days::new(n.into())),
        'w' => today.checked_sub_days(Days::new(u64::from(n) * 7)),
        'm' => today.checked_sub_months(Months::new(n)),
        'y' => today.checked_sub_months(Months::new(n.checked_mul(12)?)),
        _ => None,
    }
}

/// A stored date or time, as SQLite's `datetime()` reads the forms sclerox
/// stores: a date, or a date and time with an optional `T`, fraction and `Z`.
/// Like `datetime()`, it drops the fraction.
fn read_stamp(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim().trim_end_matches('Z').replacen('T', " ", 1);
    NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f")
        .map(|at| at.with_nanosecond(0).unwrap_or(at))
        .or_else(|_| NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M"))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                .ok()
                .map(|d| d.and_time(NaiveTime::MIN))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(from: Option<&str>, to: Option<&str>) -> Result<Dates> {
        let today = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        Dates::parse_on(from, to, false, today)
    }

    #[test]
    fn bounds_take_dates_times_and_ages_and_include_the_whole_last_day() {
        let values = |from, to| on(from, to).unwrap().values();
        assert_eq!(
            values(Some("2026-06-01"), Some("2026-06-08")),
            [
                Some("2026-06-01 00:00:00".into()),
                Some("2026-06-08 23:59:59".into())
            ]
        );
        assert_eq!(
            values(Some("2026-06-01T09:30"), Some("2026-06-01 10:00")),
            [
                Some("2026-06-01 09:30:00".into()),
                Some("2026-06-01 10:00:59".into())
            ]
        );
        assert_eq!(
            values(Some("1m"), Some("2w")),
            [
                Some("2026-02-28 00:00:00".into()),
                Some("2026-03-17 23:59:59".into())
            ]
        );
        assert_eq!(
            values(Some("1y"), Some("0d")),
            [
                Some("2025-03-31 00:00:00".into()),
                Some("2026-03-31 23:59:59".into())
            ]
        );
        assert_eq!(values(None, None), [None, None]);
    }

    #[test]
    fn unreadable_or_reversed_bounds_are_errors() {
        assert!(on(Some("last week"), None).is_err());
        assert!(on(None, Some("7x")).is_err());
        assert!(on(Some("2026-13-01"), None).is_err());
        assert!(on(Some("2026-06-08"), Some("2026-06-01")).is_err());
    }

    #[test]
    fn contains_reads_stored_dates_as_sql_does() {
        let june = on(Some("2026-06-01"), Some("2026-06-08")).unwrap();
        for inside in [
            "2026-06-01",
            "2026-06-08 23:59:59",
            "2026-06-08T12:00:00Z",
            "2026-06-04 10:00:00.250",
            "2026-06-08 23:59:59.500",
        ] {
            assert!(june.contains(Some(inside)), "{inside}");
        }
        for outside in ["2026-05-31 23:59:59", "2026-06-09", "soon"] {
            assert!(!june.contains(Some(outside)), "{outside}");
        }
        assert!(!june.contains(None));
        assert!(Dates::default().contains(None));

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let [from, to] = june.values();
        for stamp in [
            "2026-06-01",
            "2026-06-08T12:00:00Z",
            "2026-06-08 23:59:59.500",
            "2026-06-09",
            "soon",
        ] {
            let in_sql: bool = conn
                .query_row(
                    &format!("SELECT coalesce({}, 0)", june.sql("?3", 1)),
                    rusqlite::params![from, to, stamp],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(in_sql, june.contains(Some(stamp)), "{stamp}");
        }
    }
}
