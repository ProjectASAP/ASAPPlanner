pub mod duration {
    use std::fmt::{self, Formatter};

    pub fn fmt_duration_ms(f: &mut Formatter<'_>, value: i64) -> fmt::Result {
        if value == 0 {
            return write!(f, "0ms");
        }
        let mut remainder = value;
        for (unit, suffix) in [
            (31_536_000_000, "y"),
            (86_400_000, "d"),
            (3_600_000, "h"),
            (60_000, "m"),
            (1_000, "s"),
        ] {
            let part = remainder / unit;
            if part != 0 {
                write!(f, "{part}{suffix}")?;
                remainder %= unit;
            }
        }
        if remainder != 0 {
            write!(f, "{remainder}ms")?;
        }
        Ok(())
    }
}

pub mod hash {
    pub type FastHashMap<K, V> = std::collections::HashMap<K, V>;
    pub type FastHashSet<T> = std::collections::HashSet<T>;
    pub trait HashSetExt {}
    impl<T, S> HashSetExt for std::collections::HashSet<T, S> {}
}

pub mod prelude {
    pub use crate::time::{datetime_part, timestamp_secs_to_utc_datetime, DateTimePart};
}

mod time {
    use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};

    #[derive(Clone, Copy)]
    pub enum DateTimePart {
        DayOfMonth,
        DayOfWeek,
        DayOfYear,
        DaysInMonth,
        Hour,
        Minute,
        Month,
        Second,
        Year,
    }

    pub fn timestamp_secs_to_utc_datetime(secs: i64) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(secs, 0).map(|value| Utc.from_utc_datetime(&value.naive_utc()))
    }

    pub fn datetime_part<Tz: TimeZone>(value: DateTime<Tz>, part: DateTimePart) -> Option<u32> {
        Some(match part {
            DateTimePart::DayOfMonth => value.day(),
            DateTimePart::DayOfWeek => value.weekday().num_days_from_sunday(),
            DateTimePart::DayOfYear => value.ordinal(),
            DateTimePart::DaysInMonth => {
                let (year, month) = (value.year(), value.month());
                let next = if month == 12 {
                    NaiveDate::from_ymd_opt(year + 1, 1, 1)?
                } else {
                    NaiveDate::from_ymd_opt(year, month + 1, 1)?
                };
                next.signed_duration_since(NaiveDate::from_ymd_opt(year, month, 1)?)
                    .num_days() as u32
            }
            DateTimePart::Hour => value.hour(),
            DateTimePart::Minute => value.minute(),
            DateTimePart::Month => value.month(),
            DateTimePart::Second => value.second(),
            DateTimePart::Year => u32::try_from(value.year()).ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{duration::fmt_duration_ms, time};
    use chrono::{TimeZone, Utc};
    use std::fmt;

    struct Millis(i64);

    impl fmt::Display for Millis {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            super::duration::fmt_duration_ms(formatter, self.0)
        }
    }

    #[test]
    fn duration_format_matches_upstream_parser_support_cases() {
        let cases = [
            (0, "0ms"),
            (1, "1ms"),
            (1_001, "1s1ms"),
            (90_061, "1m30s61ms"),
            (31_626_061_001, "1y1d1h1m1s1ms"),
            (-1_001, "-1s-1ms"),
        ];
        for (value, expected) in cases {
            assert_eq!(Millis(value).to_string(), expected);
        }
    }

    #[test]
    fn datetime_helpers_match_upstream_parser_support_cases() {
        let epoch = time::timestamp_secs_to_utc_datetime(0).unwrap();
        assert_eq!(
            time::datetime_part(epoch, time::DateTimePart::Year),
            Some(1970)
        );
        let leap = Utc.with_ymd_and_hms(2024, 2, 29, 23, 58, 57).unwrap();
        assert_eq!(
            time::datetime_part(leap, time::DateTimePart::DaysInMonth),
            Some(29)
        );
        assert_eq!(
            time::datetime_part(leap, time::DateTimePart::DayOfWeek),
            Some(4)
        );
        let negative_year = Utc.with_ymd_and_hms(-1, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(
            time::datetime_part(negative_year, time::DateTimePart::Year),
            None
        );
    }

    #[test]
    fn hash_aliases_preserve_map_and_set_behavior() {
        let mut map = super::hash::FastHashMap::default();
        map.insert("a", 1);
        assert_eq!(map.get("a"), Some(&1));
        let mut set = super::hash::FastHashSet::default();
        set.insert("a");
        assert!(set.contains("a"));
    }

    #[allow(dead_code)]
    fn formatter_signature_is_the_upstream_signature(
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        fmt_duration_ms(formatter, 1)
    }
}
