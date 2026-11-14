//! Gregorian civil dates and conversions to Unix time.

/// One day in the proleptic Gregorian calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CivilDate {
    year: i32,
    month: u32,
    day: u32,
}

impl CivilDate {
    pub(crate) const UNIX_EPOCH: Self = Self {
        year: 1970,
        month: 1,
        day: 1,
    };

    /// Construct a valid calendar date.
    pub(crate) fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        let days_in_month = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap_year(year) => 29,
            2 => 28,
            _ => return None,
        };
        if day == 0 || day > days_in_month {
            return None;
        }
        Some(Self { year, month, day })
    }

    pub(crate) const fn year(self) -> i32 {
        self.year
    }

    /// Calendar components as `(year, month, day)`.
    pub(crate) const fn components(self) -> (i32, u32, u32) {
        (self.year, self.month, self.day)
    }

    /// Format as ISO `YYYY-MM-DD`.
    pub(crate) fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Get the UTC calendar date containing a Unix timestamp.
    pub(crate) fn from_unix_timestamp(seconds: i64) -> Option<Self> {
        let days = seconds.div_euclid(86_400);
        let shifted_days = days.checked_add(719_468)?;
        let era = if shifted_days >= 0 {
            shifted_days
        } else {
            shifted_days.checked_sub(146_096)?
        } / 146_097;
        let day_of_era = shifted_days.checked_sub(era.checked_mul(146_097)?)?;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let mut year = year_of_era.checked_add(era.checked_mul(400)?)?;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_prime = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
        let month = month_prime + if month_prime < 10 { 3 } else { -9 };
        year = year.checked_add(if month <= 2 { 1 } else { 0 })?;

        Self::new(
            i32::try_from(year).ok()?,
            u32::try_from(month).ok()?,
            u32::try_from(day).ok()?,
        )
    }
}

const fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}
