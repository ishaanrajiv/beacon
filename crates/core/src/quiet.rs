//! Local time: quiet hours and the local day (`spec/delivery.md`, "The numbers"). Quiet hours are a
//! window of wall-clock time in the configured timezone, `[start, end)`, that may cross midnight.
//! A daylight-saving change moves the instants, never the wall-clock times: quiet hours from 23:00
//! to 08:00 end at 08:00 local time on the morning the clocks change too.

use jiff::Timestamp;
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;

/// A time of day, `HH:MM`, as minutes after midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClockTime(u16);

impl ClockTime {
    pub fn new(hour: u8, minute: u8) -> Option<ClockTime> {
        (hour < 24 && minute < 60).then(|| ClockTime(u16::from(hour) * 60 + u16::from(minute)))
    }

    /// `HH:MM`, 24-hour, two digits each.
    pub fn parse(text: &str) -> Result<ClockTime, String> {
        let bad = || format!("`{text}` is not a time of day; write HH:MM, 24-hour, like 23:00 or 07:30");
        let (h, m) = text.split_once(':').ok_or_else(bad)?;
        if h.len() != 2 || m.len() != 2 {
            return Err(bad());
        }
        let hour: u8 = h.parse().map_err(|_| bad())?;
        let minute: u8 = m.parse().map_err(|_| bad())?;
        ClockTime::new(hour, minute).ok_or_else(bad)
    }

    pub fn hour(self) -> u8 {
        u8::try_from(self.0 / 60).unwrap_or(0)
    }

    pub fn minute(self) -> u8 {
        u8::try_from(self.0 % 60).unwrap_or(0)
    }

    fn civil(self) -> Time {
        Time::constant(self.hour() as i8, self.minute() as i8, 0, 0)
    }
}

impl std::fmt::Display for ClockTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour(), self.minute())
    }
}

/// `[start, end)` in local time. Equal start and end turn quiet hours off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    pub start: ClockTime,
    pub end: ClockTime,
}

impl Default for QuietHours {
    /// 23:00 to 08:00.
    fn default() -> QuietHours {
        QuietHours {
            start: ClockTime(23 * 60),
            end: ClockTime(8 * 60),
        }
    }
}

impl QuietHours {
    pub fn is_off(&self) -> bool {
        self.start == self.end
    }

    /// Whether `now_ms` falls within quiet hours in `tz`.
    pub fn contains(&self, now_ms: i64, tz: &TimeZone) -> bool {
        if self.is_off() {
            return false;
        }
        let Ok(now) = Timestamp::from_millisecond(now_ms) else {
            return false;
        };
        let local = now.to_zoned(tz.clone()).time();
        let (start, end) = (self.start.civil(), self.end.civil());
        if start < end {
            start <= local && local < end
        } else {
            local >= start || local < end
        }
    }

    /// The first instant after `now_ms` at which local time reads `end`: when a quiet-hours hold
    /// is released and the catch-up goes out. None when quiet hours are off. On a day whose clocks
    /// skip `end` (it falls in a daylight-saving gap), the instant the gap ends; on a day that
    /// reads `end` twice, the first.
    pub fn next_end(&self, now_ms: i64, tz: &TimeZone) -> Option<i64> {
        if self.is_off() {
            return None;
        }
        next_local(self.end, now_ms, tz)
    }

    /// The first instant after `now_ms` at which local time reads `start`.
    pub fn next_start(&self, now_ms: i64, tz: &TimeZone) -> Option<i64> {
        if self.is_off() {
            return None;
        }
        next_local(self.start, now_ms, tz)
    }
}

fn next_local(at: ClockTime, now_ms: i64, tz: &TimeZone) -> Option<i64> {
    let now = Timestamp::from_millisecond(now_ms).ok()?;
    let mut date = now.to_zoned(tz.clone()).date();
    // Two days always reach it; three covers a zone that skips a whole day.
    for _ in 0..3 {
        let instant = date
            .to_datetime(at.civil())
            .to_zoned(tz.clone())
            .ok()?
            .timestamp();
        let ms = instant.as_millisecond();
        if ms > now_ms {
            return Some(ms);
        }
        date = date.tomorrow().ok()?;
    }
    None
}

/// The local day `now_ms` falls in, `YYYY-MM-DD`: the budget's day.
pub fn local_day(now_ms: i64, tz: &TimeZone) -> String {
    match Timestamp::from_millisecond(now_ms) {
        Ok(now) => now.to_zoned(tz.clone()).date().to_string(),
        Err(_) => Date::default().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn ms(tz: &TimeZone, y: i16, mo: i8, d: i8, h: i8, mi: i8) -> i64 {
        date(y, mo, d)
            .at(h, mi, 0, 0)
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_millisecond()
    }

    fn q(start: &str, end: &str) -> QuietHours {
        QuietHours {
            start: ClockTime::parse(start).unwrap(),
            end: ClockTime::parse(end).unwrap(),
        }
    }

    #[test]
    fn parses_clock_times() {
        assert_eq!(ClockTime::parse("23:00").unwrap().to_string(), "23:00");
        assert_eq!(ClockTime::parse("07:05").unwrap().minute(), 5);
        for bad in ["24:00", "7:00", "07:60", "0700", "ab:cd", ""] {
            let message = ClockTime::parse(bad).unwrap_err();
            assert!(message.contains("HH:MM"), "{bad}: {message}");
        }
    }

    #[test]
    fn a_window_crossing_midnight() {
        let utc = TimeZone::UTC;
        let quiet = q("23:00", "08:00");
        assert!(!quiet.contains(ms(&utc, 2026, 11, 13, 22, 59), &utc));
        assert!(quiet.contains(ms(&utc, 2026, 11, 13, 23, 0), &utc));
        assert!(quiet.contains(ms(&utc, 2026, 11, 14, 0, 0), &utc));
        assert!(quiet.contains(ms(&utc, 2026, 11, 14, 7, 59), &utc));
        assert!(!quiet.contains(ms(&utc, 2026, 11, 14, 8, 0), &utc));
        assert!(!quiet.contains(ms(&utc, 2026, 11, 14, 12, 0), &utc));
        // From 23:30 the end is tomorrow 08:00; from 03:00 it is today's.
        assert_eq!(
            quiet.next_end(ms(&utc, 2026, 11, 13, 23, 30), &utc),
            Some(ms(&utc, 2026, 11, 14, 8, 0))
        );
        assert_eq!(
            quiet.next_end(ms(&utc, 2026, 11, 14, 3, 0), &utc),
            Some(ms(&utc, 2026, 11, 14, 8, 0))
        );
        // At exactly 08:00 the next end is tomorrow's.
        assert_eq!(
            quiet.next_end(ms(&utc, 2026, 11, 14, 8, 0), &utc),
            Some(ms(&utc, 2026, 11, 15, 8, 0))
        );
    }

    #[test]
    fn a_window_within_a_day() {
        let utc = TimeZone::UTC;
        let quiet = q("13:00", "14:30");
        assert!(!quiet.contains(ms(&utc, 2026, 11, 13, 12, 59), &utc));
        assert!(quiet.contains(ms(&utc, 2026, 11, 13, 13, 0), &utc));
        assert!(quiet.contains(ms(&utc, 2026, 11, 13, 14, 29), &utc));
        assert!(!quiet.contains(ms(&utc, 2026, 11, 13, 14, 30), &utc));
        assert_eq!(
            quiet.next_start(ms(&utc, 2026, 11, 13, 12, 0), &utc),
            Some(ms(&utc, 2026, 11, 13, 13, 0))
        );
    }

    #[test]
    fn equal_start_and_end_is_off() {
        let utc = TimeZone::UTC;
        let quiet = q("08:00", "08:00");
        assert!(quiet.is_off());
        assert!(!quiet.contains(ms(&utc, 2026, 11, 13, 8, 0), &utc));
        assert_eq!(quiet.next_end(0, &utc), None);
    }

    #[test]
    fn local_time_in_another_zone() {
        let tz = TimeZone::get("Asia/Kolkata").unwrap();
        let quiet = q("23:00", "08:00");
        // 18:00 UTC is 23:30 in Kolkata.
        let utc = TimeZone::UTC;
        assert!(quiet.contains(ms(&utc, 2026, 11, 13, 18, 0), &tz));
        assert!(!quiet.contains(ms(&utc, 2026, 11, 13, 17, 0), &tz));
        assert_eq!(
            quiet.next_end(ms(&utc, 2026, 11, 13, 18, 0), &tz),
            Some(ms(&tz, 2026, 11, 14, 8, 0))
        );
    }

    #[test]
    fn daylight_saving_moves_the_instant_not_the_wall_clock() {
        let tz = TimeZone::get("Europe/London").unwrap();
        let quiet = q("23:00", "08:00");
        // The clocks go back at 02:00 BST on 2026-10-25: the night is an hour longer.
        let start = ms(&tz, 2026, 10, 24, 23, 0);
        let end = quiet.next_end(start, &tz).unwrap();
        assert_eq!(end - start, 10 * 3_600_000);
        assert_eq!(end, ms(&tz, 2026, 10, 25, 8, 0));
        // The clocks go forward at 01:00 GMT on 2026-03-29: the night is an hour shorter.
        let start = ms(&tz, 2026, 3, 28, 23, 0);
        let end = quiet.next_end(start, &tz).unwrap();
        assert_eq!(end - start, 8 * 3_600_000);
        // 01:30 on the morning the clocks go back happens twice; both are quiet.
        let first = ms(&tz, 2026, 10, 25, 1, 30);
        assert!(quiet.contains(first, &tz));
        assert!(quiet.contains(first + 3_600_000, &tz));
    }

    #[test]
    fn an_end_in_the_gap_is_when_the_gap_ends() {
        let tz = TimeZone::get("Europe/London").unwrap();
        // 01:30 doesn't exist on 2026-03-29 in London: the clocks jump from 01:00 to 02:00.
        let quiet = q("23:00", "01:30");
        let start = ms(&tz, 2026, 3, 28, 23, 0);
        let end = quiet.next_end(start, &tz).unwrap();
        assert_eq!(end, ms(&tz, 2026, 3, 29, 2, 30));
        assert!(!quiet.contains(end, &tz));
    }

    #[test]
    fn local_days() {
        let tz = TimeZone::get("America/New_York").unwrap();
        let utc = TimeZone::UTC;
        // 03:00 UTC on the 14th is still the 13th in New York.
        assert_eq!(local_day(ms(&utc, 2026, 11, 14, 3, 0), &tz), "2026-11-13");
        assert_eq!(local_day(ms(&utc, 2026, 11, 14, 3, 0), &utc), "2026-11-14");
    }
}
