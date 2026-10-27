//! Wall-clock time for the home screen.
//!
//! RustOS keeps UTC and has no timezone database, so local time is UTC plus a
//! fixed offset from the runtime UI configuration (`clock.utc_offset_minutes`). The calendar math is
//! pure so it can be tested without a clock.

use std::time::{SystemTime, UNIX_EPOCH};

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A local time of day and calendar date, down to the minute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClockReading {
    pub(crate) hour: u8,
    pub(crate) minute: u8,
    /// 0 is Sunday.
    pub(crate) weekday: u8,
    /// 1 is January.
    pub(crate) month: u8,
    pub(crate) day: u8,
    /// Whole local minutes since the Unix epoch. It changes exactly when the
    /// displayed time does, so it says when the clock needs redrawing.
    pub(crate) minute_key: i64,
}

impl ClockReading {
    /// The configured local time now, or `None` if the system clock is before
    /// the Unix epoch.
    pub(crate) fn now() -> Option<Self> {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())?;
        Some(Self::from_unix(
            seconds,
            crate::ui_assets::clock_utc_offset_minutes(),
        ))
    }

    /// The local time for `unix_seconds` at `utc_offset_minutes` east of UTC.
    pub(crate) fn from_unix(unix_seconds: i64, utc_offset_minutes: i32) -> Self {
        let local_seconds = unix_seconds + i64::from(utc_offset_minutes) * 60;
        let days = local_seconds.div_euclid(86_400);
        let seconds_of_day = local_seconds.rem_euclid(86_400);
        let (_, month, day) = civil_from_days(days);
        Self {
            hour: (seconds_of_day / 3_600) as u8,
            minute: (seconds_of_day % 3_600 / 60) as u8,
            // 1970-01-01 was a Thursday.
            weekday: (days + 4).rem_euclid(7) as u8,
            month,
            day,
            minute_key: local_seconds.div_euclid(60),
        }
    }

    /// `HH:MM` on a 24-hour clock.
    pub(crate) fn time_text(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }

    /// A short date such as `Tue, Oct 28`.
    pub(crate) fn date_text(&self) -> String {
        format!(
            "{}, {} {}",
            WEEKDAYS[usize::from(self.weekday) % 7],
            MONTHS[usize::from(self.month - 1) % 12],
            self.day
        )
    }
}

/// Year, month and day of a count of days since 1970-01-01 in the proleptic
/// Gregorian calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u8;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u8;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_a_thursday_at_midnight_in_utc() {
        let reading = ClockReading::from_unix(0, 0);
        assert_eq!(
            (reading.hour, reading.minute, reading.weekday, reading.month, reading.day),
            (0, 0, 4, 1, 1)
        );
        assert_eq!(reading.time_text(), "00:00");
        assert_eq!(reading.date_text(), "Thu, Jan 1");
    }

    #[test]
    fn a_logged_wall_clock_second_reads_correctly_in_korea() {
        // The uiserver log stamped 1793108046 s; that was 13:34:06 UTC on
        // Tuesday 2026-10-27, which is 22:34 in Korea (UTC+9).
        let reading = ClockReading::from_unix(1_793_108_046, 540);
        assert_eq!(reading.time_text(), "22:34");
        assert_eq!(reading.date_text(), "Tue, Oct 27");
        assert_eq!(ClockReading::from_unix(1_793_108_046, 0).time_text(), "13:34");
    }

    #[test]
    fn the_offset_can_roll_the_date_forward_or_back() {
        // 23:30 UTC on 2026-10-27 is already the 28th in Korea...
        let late = 1_793_108_046 - (13 * 3_600 + 34 * 60 + 6) + 23 * 3_600 + 30 * 60;
        assert_eq!(ClockReading::from_unix(late, 540).date_text(), "Wed, Oct 28");
        assert_eq!(ClockReading::from_unix(late, 540).time_text(), "08:30");
        // ...and 01:00 UTC is still the previous evening in New York (UTC-5).
        let early = late + 90 * 60;
        assert_eq!(ClockReading::from_unix(early, -300).date_text(), "Tue, Oct 27");
        assert_eq!(ClockReading::from_unix(early, -300).time_text(), "20:00");
    }

    #[test]
    fn leap_days_and_year_boundaries_are_handled() {
        // 2024-02-29 12:00 UTC.
        let leap = ClockReading::from_unix(1_709_208_000, 0);
        assert_eq!((leap.month, leap.day), (2, 29));
        assert_eq!(leap.date_text(), "Thu, Feb 29");
        // 2000-01-01 00:00 UTC was a Saturday, and 1999-12-31 23:59:59 the Friday before.
        assert_eq!(ClockReading::from_unix(946_684_800, 0).date_text(), "Sat, Jan 1");
        assert_eq!(ClockReading::from_unix(946_684_799, 0).date_text(), "Fri, Dec 31");
        // Times before the epoch still land on a real date.
        assert_eq!(ClockReading::from_unix(-1, 0).date_text(), "Wed, Dec 31");
        assert_eq!(ClockReading::from_unix(-1, 0).time_text(), "23:59");
    }

    #[test]
    fn the_minute_key_changes_exactly_when_the_displayed_time_does() {
        let a = ClockReading::from_unix(1_793_108_040, 540);
        let b = ClockReading::from_unix(1_793_108_059, 540);
        let c = ClockReading::from_unix(1_793_108_100, 540);
        assert_eq!(a.minute_key, b.minute_key, "same minute");
        assert_eq!(a.time_text(), b.time_text());
        assert_eq!(c.minute_key, a.minute_key + 1, "next minute");
        assert_ne!(a.time_text(), c.time_text());
    }

    #[test]
    fn the_shipped_offset_is_inside_the_valid_range() {
        let offset = crate::ui_assets::clock_utc_offset_minutes();
        assert!((-720..=840).contains(&offset), "{offset}");
    }
}
