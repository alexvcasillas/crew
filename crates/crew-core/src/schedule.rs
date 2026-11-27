//! Schedule math for repeating jobs: next slot, missed slots and display text.

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveTime, TimeZone, Weekday};
use serde::{Deserialize, Serialize};

const MISSED_CAP: u32 = 10_000;
/// How many days ahead we search before giving up on an unsatisfiable repeat.
const SEARCH_DAYS: i64 = 14;

/// A set of weekdays stored as a bitmask (bit 0 = Monday).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeekdaySet(u8);

impl WeekdaySet {
    pub fn from_days(days: &[Weekday]) -> Self {
        let mut bits = 0u8;
        for d in days {
            bits |= 1 << d.num_days_from_monday();
        }
        Self(bits)
    }

    pub fn contains(&self, day: Weekday) -> bool {
        self.0 & (1 << day.num_days_from_monday()) != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    /// Every N minutes, on a grid anchored at local midnight. `window` is
    /// inclusive of start and exclusive of end.
    Every {
        minutes: u16,
        days: Option<WeekdaySet>,
        window: Option<(NaiveTime, NaiveTime)>,
    },
    Daily {
        at: NaiveTime,
    },
    Weekly {
        days: WeekdaySet,
        at: NaiveTime,
    },
}

/// Resolve a local wall-clock time. Ambiguous times use the earliest; with
/// `shift_gaps`, a skipped time moves to the next valid minute, otherwise None.
fn resolve<Tz: TimeZone>(
    tz: &Tz,
    date: NaiveDate,
    time: NaiveTime,
    shift_gaps: bool,
) -> Option<DateTime<Tz>> {
    let mut naive = date.and_time(time);
    for _ in 0..=180 {
        match tz.from_local_datetime(&naive) {
            LocalResult::Single(dt) => return Some(dt),
            LocalResult::Ambiguous(earliest, _) => return Some(earliest),
            LocalResult::None if shift_gaps => naive += Duration::minutes(1),
            LocalResult::None => return None,
        }
    }
    None
}

fn in_window(window: Option<(NaiveTime, NaiveTime)>, t: NaiveTime) -> bool {
    match window {
        None => true,
        Some((start, end)) if start <= end => t >= start && t < end,
        Some((start, end)) => t >= start || t < end,
    }
}

/// The first slot strictly after `after`.
pub fn next_after<Tz: TimeZone>(repeat: &Repeat, after: DateTime<Tz>) -> DateTime<Tz> {
    let tz = after.timezone();
    let first = after.date_naive();
    for offset in 0..SEARCH_DAYS {
        let Some(date) = first.checked_add_signed(Duration::days(offset)) else {
            break;
        };
        let weekday = date.weekday();
        match *repeat {
            Repeat::Daily { at } => {
                if let Some(dt) = resolve(&tz, date, at, true).filter(|d| *d > after) {
                    return dt;
                }
            }
            Repeat::Weekly { days, at } => {
                if days.contains(weekday)
                    && let Some(dt) = resolve(&tz, date, at, true).filter(|d| *d > after)
                {
                    return dt;
                }
            }
            Repeat::Every {
                minutes,
                days,
                window,
            } => {
                if days.is_some_and(|d| !d.contains(weekday)) {
                    continue;
                }
                let step = u32::from(minutes.max(1));
                let mut m = 0u32;
                while m < 24 * 60 {
                    let t = NaiveTime::from_hms_opt(m / 60, m % 60, 0).expect("valid time");
                    if in_window(window, t)
                        && let Some(dt) = resolve(&tz, date, t, false).filter(|d| *d > after)
                    {
                        return dt;
                    }
                    m += step;
                }
            }
        }
    }
    after + Duration::days(1)
}

/// The number of slots in `(from, to]`, capped at 10_000.
pub fn missed_between<Tz: TimeZone>(repeat: &Repeat, from: DateTime<Tz>, to: DateTime<Tz>) -> u32 {
    let mut count = 0;
    let mut cur = next_after(repeat, from);
    while cur <= to && count < MISSED_CAP {
        count += 1;
        cur = next_after(repeat, cur);
    }
    count
}

fn time_text(t: NaiveTime) -> String {
    use chrono::Timelike;
    format!("{}:{:02}", t.hour(), t.minute())
}

fn day_list(days: WeekdaySet) -> String {
    const ALL: [Weekday; 7] = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];
    if days == WeekdaySet::from_days(&ALL[..5]) {
        return "Weekdays".into();
    }
    if days == WeekdaySet::from_days(&ALL[5..]) {
        return "Weekends".into();
    }
    if days == WeekdaySet::from_days(&ALL) {
        return "Every day".into();
    }
    ALL.iter()
        .filter(|d| days.contains(**d))
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn plural_day(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Mondays",
        Weekday::Tue => "Tuesdays",
        Weekday::Wed => "Wednesdays",
        Weekday::Thu => "Thursdays",
        Weekday::Fri => "Fridays",
        Weekday::Sat => "Saturdays",
        Weekday::Sun => "Sundays",
    }
}

/// The two lines the page shows for a repeat.
pub fn describe(repeat: &Repeat) -> (String, String) {
    match *repeat {
        Repeat::Every {
            minutes,
            days,
            window,
        } => {
            let first = match minutes {
                60 => "Every hour".to_string(),
                m if m > 60 && m % 60 == 0 => format!("Every {} hours", m / 60),
                m => format!("Every {m} min"),
            };
            let window = window.map(|(s, e)| format!("{} to {}", time_text(s), time_text(e)));
            let second = match (days.map(day_list), window) {
                (None, None) => "Any time".to_string(),
                (Some(d), None) => d,
                (None, Some(w)) => w,
                (Some(d), Some(w)) => format!("{d}, {w}"),
            };
            (first, second)
        }
        Repeat::Daily { at } => (format!("Every day at {}", time_text(at)), String::new()),
        Repeat::Weekly { days, at } => {
            let single = [
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
                Weekday::Sat,
                Weekday::Sun,
            ]
            .into_iter()
            .filter(|d| days.contains(*d))
            .collect::<Vec<_>>();
            let label = match single.as_slice() {
                [d] => plural_day(*d).to_string(),
                _ => day_list(days),
            };
            (format!("{label} at {}", time_text(at)), String::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::schedule::{Repeat, WeekdaySet, describe, missed_between, next_after};
    use chrono::{DateTime, NaiveTime, TimeZone, Weekday};
    use chrono_tz::{Europe::Madrid, Tz};

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Tz> {
        Madrid.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn weekdays() -> WeekdaySet {
        WeekdaySet::from_days(&[
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
        ])
    }

    #[test]
    fn every_hour_lands_on_the_hour() {
        let r = Repeat::Every {
            minutes: 60,
            days: None,
            window: None,
        };
        assert_eq!(
            next_after(&r, at(2026, 11, 25, 14, 20, 0)),
            at(2026, 11, 25, 15, 0, 0)
        );
    }

    #[test]
    fn every_30_with_window_and_weekdays() {
        let r = Repeat::Every {
            minutes: 30,
            days: Some(weekdays()),
            window: Some((t(9, 0), t(19, 0))),
        };
        // Friday 2026-11-27 19:10 -> Monday 09:00.
        assert_eq!(
            next_after(&r, at(2026, 11, 27, 19, 10, 0)),
            at(2026, 11, 30, 9, 0, 0)
        );
        // The window end is exclusive.
        assert_eq!(
            next_after(&r, at(2026, 11, 27, 18, 30, 0)),
            at(2026, 11, 30, 9, 0, 0)
        );
    }

    #[test]
    fn daily_and_weekly() {
        let daily = Repeat::Daily { at: t(14, 45) };
        assert_eq!(
            next_after(&daily, at(2026, 11, 25, 14, 45, 0)),
            at(2026, 11, 26, 14, 45, 0)
        );
        let weekly = Repeat::Weekly {
            days: WeekdaySet::from_days(&[Weekday::Mon]),
            at: t(9, 0),
        };
        // Wednesday 2026-11-25 -> Monday 2026-11-30.
        assert_eq!(
            next_after(&weekly, at(2026, 11, 25, 15, 20, 0)),
            at(2026, 11, 30, 9, 0, 0)
        );
    }

    #[test]
    fn skipped_local_time_runs_at_next_valid_minute() {
        let r = Repeat::Daily { at: t(2, 30) };
        // Madrid springs forward on 2026-03-29: 02:00 becomes 03:00.
        assert_eq!(
            next_after(&r, at(2026, 3, 28, 12, 0, 0)),
            at(2026, 3, 29, 3, 0, 0)
        );
    }

    #[test]
    fn missed_counts_slots() {
        let r = Repeat::Every {
            minutes: 60,
            days: None,
            window: None,
        };
        assert_eq!(
            missed_between(&r, at(2026, 11, 25, 23, 0, 0), at(2026, 11, 26, 8, 14, 0)),
            9
        );
    }

    #[test]
    fn describe_matches_the_board() {
        let every = |minutes, days, window| Repeat::Every {
            minutes,
            days,
            window,
        };
        assert_eq!(
            describe(&every(30, Some(weekdays()), Some((t(9, 0), t(19, 0))))),
            ("Every 30 min".into(), "Weekdays, 9:00 to 19:00".into())
        );
        assert_eq!(
            describe(&every(60, None, None)),
            ("Every hour".into(), "Any time".into())
        );
        assert_eq!(describe(&every(15, None, None)).0, "Every 15 min");
        assert_eq!(describe(&every(120, None, None)).0, "Every 2 hours");
        assert_eq!(describe(&every(360, None, None)).0, "Every 6 hours");
        assert_eq!(
            describe(&every(60, None, Some((t(9, 0), t(19, 0))))).1,
            "9:00 to 19:00"
        );
        let weekend = WeekdaySet::from_days(&[Weekday::Sat, Weekday::Sun]);
        assert_eq!(describe(&every(60, Some(weekend), None)).1, "Weekends");
        let mwf = WeekdaySet::from_days(&[Weekday::Mon, Weekday::Wed, Weekday::Fri]);
        assert_eq!(describe(&every(60, Some(mwf), None)).1, "Mon, Wed, Fri");
        assert_eq!(
            describe(&Repeat::Daily { at: t(14, 45) }).0,
            "Every day at 14:45"
        );
        let weekly = |days: &[Weekday]| Repeat::Weekly {
            days: WeekdaySet::from_days(days),
            at: t(9, 0),
        };
        assert_eq!(describe(&weekly(&[Weekday::Mon])).0, "Mondays at 9:00");
        assert_eq!(
            describe(&weekly(&[Weekday::Mon, Weekday::Thu])).0,
            "Mon, Thu at 9:00"
        );
        assert_eq!(
            describe(&weekly(&[
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri
            ]))
            .0,
            "Weekdays at 9:00"
        );
    }

    #[test]
    fn repeat_round_trips_through_json() {
        let r = Repeat::Every {
            minutes: 30,
            days: Some(weekdays()),
            window: Some((t(9, 0), t(19, 0))),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Repeat>(&json).unwrap(), r);
    }
}
