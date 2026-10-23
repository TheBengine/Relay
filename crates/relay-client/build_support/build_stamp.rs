//! UTC build identifiers shared by the build script and support information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildStamp {
    pub sequence: String,
    pub utc: String,
    pub date: String,
    pub display: String,
}

impl BuildStamp {
    pub fn parse(sequence: &str) -> Result<Self, &'static str> {
        if sequence.len() != 14 || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("Build sequence must contain fourteen UTC digits");
        }
        let value = |range: std::ops::Range<usize>| sequence[range].parse::<u32>().unwrap();
        let (year, month, day) = (value(0..4), value(4..6), value(6..8));
        let (hour, minute, second) = (value(8..10), value(10..12), value(12..14));
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return Err("Build sequence contains an invalid UTC month"),
        };
        if !(1970..=9999).contains(&year)
            || !(1..=days).contains(&day)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return Err("Build sequence contains an invalid UTC date or time");
        }
        let month_name = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ][month as usize - 1];
        Ok(Self {
            sequence: sequence.into(),
            utc: format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"),
            date: format!("{year:04}-{month:02}-{day:02}"),
            display: format!("{day} {month_name} {year:04}"),
        })
    }

    pub fn from_unix_seconds(seconds: u64) -> Result<Self, &'static str> {
        // Proleptic Gregorian civil-date conversion for nonnegative Unix time.
        let days = i64::try_from(seconds / 86_400).map_err(|_| "Build time exceeds its limit")?;
        let shifted = days
            .checked_add(719_468)
            .ok_or("Build time exceeds its limit")?;
        let era = shifted / 146_097;
        let day_of_era = shifted - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let year = year_of_era + era * 400;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_index = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_index + 2) / 5 + 1;
        let month = month_index + if month_index < 10 { 3 } else { -9 };
        let year = year + i64::from(month <= 2);
        if year > 9999 {
            return Err("Build time exceeds year 9999");
        }
        let time = seconds % 86_400;
        Self::parse(&format!(
            "{year:04}{month:02}{day:02}{:02}{:02}{:02}",
            time / 3600,
            time % 3600 / 60,
            time % 60
        ))
    }

    pub fn resolve(
        sequence: Option<&str>,
        source_date_epoch: Option<&str>,
        now: u64,
    ) -> Result<Self, &'static str> {
        let explicit = sequence.map(Self::parse).transpose()?;
        let reproducible = source_date_epoch
            .map(|value| {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err("SOURCE_DATE_EPOCH must be nonnegative Unix seconds");
                }
                Self::from_unix_seconds(value.parse().map_err(|_| "Invalid SOURCE_DATE_EPOCH")?)
            })
            .transpose()?;
        match (explicit, reproducible) {
            (Some(sequence), Some(epoch)) if sequence != epoch => {
                Err("RELAY_BUILD_SEQUENCE and SOURCE_DATE_EPOCH disagree")
            }
            (Some(stamp), _) | (_, Some(stamp)) => Ok(stamp),
            (None, None) => Self::from_unix_seconds(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_midnight_leap_days_and_exact_seconds_are_preserved() {
        let before = BuildStamp::from_unix_seconds(1_792_022_399).unwrap();
        let after = BuildStamp::from_unix_seconds(1_792_022_400).unwrap();
        assert_eq!(before.sequence, "20261014235959");
        assert_eq!(after.sequence, "20261015000000");
        assert_eq!(after.utc, "2026-10-15T00:00:00Z");
        assert_eq!(after.display, "15 Oct 2026");
        assert_eq!(
            BuildStamp::from_unix_seconds(0).unwrap().sequence,
            "19700101000000"
        );
        assert!(BuildStamp::parse("20000229010203").is_ok());
        assert!(BuildStamp::parse("21000229010203").is_err());
    }

    #[test]
    fn malformed_or_conflicting_build_identifiers_fail_instead_of_showing_a_false_date() {
        for invalid in [
            "",
            "20261015",
            "202610150000000",
            "20261301000000",
            "20260231000000",
            "20261015000060",
            "20261015240000",
            "20261015006000",
            "19691231235959",
            "２０２６１０１５００００００",
            "2026101500000x",
            "20261015000000\n",
        ] {
            assert!(BuildStamp::parse(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(BuildStamp::from_unix_seconds(u64::MAX).is_err());
        assert!(BuildStamp::resolve(None, Some("-1"), 0).is_err());
        assert!(BuildStamp::resolve(None, Some("x"), 0).is_err());
        assert!(BuildStamp::resolve(Some("20261015000000"), Some("0"), 0).is_err());
        assert_eq!(
            BuildStamp::resolve(Some("20261015000000"), Some("1792022400"), 0)
                .unwrap()
                .sequence,
            "20261015000000"
        );
    }
}
