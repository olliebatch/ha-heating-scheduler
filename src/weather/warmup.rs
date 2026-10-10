use super::Weather;
use super::early::seconds_until;
use crate::schedule::{HeatingState, Schedule};
use chrono::{Duration, NaiveTime};
use serde::{Deserialize, Serialize};

/// A zone's cold-day warm-ups: short bursts of heating partway through a long Off gap when it's
/// very cold outside. Off by default; zones without them stay off for the whole of an Off gap.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColdWarmups {
    pub enabled: bool,
    /// Warm up only when it's colder than this outside (°C)
    pub below_c: f64,
    /// Only in Off gaps longer than this
    pub min_gap_minutes: u32,
    /// How long each warm-up lasts
    pub burst_minutes: u32,
    /// One warm-up this long after the gap starts, then again every this many minutes
    pub every_minutes: u32,
}

impl Default for ColdWarmups {
    fn default() -> Self {
        ColdWarmups {
            enabled: false,
            below_c: 0.0,
            min_gap_minutes: 240,
            burst_minutes: 20,
            every_minutes: 180,
        }
    }
}

const DAY_MINUTES: u32 = 24 * 60;
/// Largest allowed `burst_minutes`
const MAX_BURST: u32 = 180;
/// Allowed `below_c` (°C)
const BELOW_C_RANGE: std::ops::RangeInclusive<f64> = -30.0..=20.0;

impl ColdWarmups {
    /// Why these settings can't be used, if they can't
    pub fn validate(&self) -> Result<(), String> {
        let within = |name: &str, value: u32, max: u32| {
            if value == 0 || value > max {
                Err(format!("cold_warmups.{name} must be 1-{max}"))
            } else {
                Ok(())
            }
        };
        within("min_gap_minutes", self.min_gap_minutes, DAY_MINUTES)?;
        within("every_minutes", self.every_minutes, DAY_MINUTES)?;
        within("burst_minutes", self.burst_minutes, MAX_BURST)?;
        if self.burst_minutes >= self.every_minutes {
            return Err("cold_warmups.burst_minutes must be less than every_minutes".to_string());
        }
        if !BELOW_C_RANGE.contains(&self.below_c) {
            return Err(format!(
                "cold_warmups.below_c must be {} to {} °C",
                BELOW_C_RANGE.start(),
                BELOW_C_RANGE.end()
            ));
        }
        Ok(())
    }
}

/// A warm-up in progress
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WarmUp {
    pub started_at: NaiveTime,
    pub until: NaiveTime,
    /// The outside temperature when it started (°C), the reading it was decided on
    pub outside_c: f64,
}

/// How soon after a warm-up's slot it can begin, in seconds: a scheduler tick (15 s) with room
/// to spare. The weather is checked once, at the start; a day turning colder mid-slot waits for
/// the next one.
const DECIDE_WITHIN: u32 = 60;

/// The Off gap `now` is in: when it started and how long it is, in seconds. With no On periods at
/// all, the whole day from midnight.
fn off_gap(schedule: &Schedule, now: NaiveTime) -> Option<(NaiveTime, u32)> {
    let entry = schedule
        .entries
        .iter()
        .find(|e| e.time_period.contains(now))?;
    if entry.heating_state == HeatingState::On {
        return None;
    }
    let period = &entry.time_period;
    let length = if period.is_full_day() {
        DAY_MINUTES * 60
    } else {
        seconds_until(period.start, period.end)
    };
    Some((period.start, length))
}

fn on_at(schedule: &Schedule, now: NaiveTime) -> bool {
    schedule
        .entries
        .iter()
        .any(|e| e.heating_state == HeatingState::On && e.time_period.contains(now))
}

/// A warm-up starting now, if one is due: inside an Off gap longer than `min_gap_minutes`, at
/// the gap's start plus `every_minutes` (and each `every_minutes` after), when it's colder than
/// `below_c` outside. A warm-up that would overlap the late finish that may follow the previous
/// On period (`after_on` minutes) or the early start of the next one (`before_on` minutes) is
/// skipped. With no weather reading, there's no warm-up.
pub fn warm_up(
    schedule: &Schedule,
    now: NaiveTime,
    weather: Option<&Weather>,
    warmups: &ColdWarmups,
    before_on: u32,
    after_on: u32,
) -> Option<WarmUp> {
    if !warmups.enabled {
        return None;
    }
    let (gap_start, gap) = off_gap(schedule, now)?;
    if gap <= warmups.min_gap_minutes * 60 {
        return None;
    }
    let (every, burst) = (warmups.every_minutes * 60, warmups.burst_minutes * 60);
    let into_gap = seconds_until(gap_start, now);
    // The latest slot at or before now; slot 0 is the gap's start, which isn't a warm-up
    let slot = into_gap / every * every;
    if slot == 0 || into_gap - slot >= DECIDE_WITHIN {
        return None;
    }
    if slot < after_on * 60 || slot + burst > gap.saturating_sub(before_on * 60) {
        return None;
    }
    let outside = weather?.temperature?;
    let started_at = gap_start + Duration::seconds(i64::from(slot));
    (outside < warmups.below_c).then(|| WarmUp {
        started_at,
        until: started_at + Duration::seconds(i64::from(burst)),
        outside_c: outside,
    })
}

/// Keep a warm-up that has begun going for its burst, whatever the weather does; None once it's
/// over or the schedule turned On
pub fn continue_warm_up(schedule: &Schedule, now: NaiveTime, begun: &WarmUp) -> Option<WarmUp> {
    let burst = seconds_until(begun.started_at, begun.until);
    (!on_at(schedule, now) && seconds_until(begun.started_at, now) < burst).then(|| begun.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{ScheduleEntry, TimePeriod};

    fn at(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
    }

    fn outside(temperature: f64) -> Weather {
        Weather {
            temperature: Some(temperature),
            wind_speed: None,
            cloud_coverage: None,
        }
    }

    fn schedule(on: &[(u32, u32, u32, u32)]) -> Schedule {
        let mut schedule = Schedule::new("Test");
        for &(sh, sm, eh, em) in on {
            schedule.add_entry(
                ScheduleEntry::new("On", TimePeriod::new(sh, sm, eh, em), HeatingState::On)
                    .with_target(21.0),
            );
        }
        schedule
    }

    fn on() -> ColdWarmups {
        ColdWarmups {
            enabled: true,
            ..ColdWarmups::default()
        }
    }

    #[test]
    fn test_warm_up_table() {
        // Off 09:00-17:00 (8 h) and, overnight, 22:00-06:00 (8 h)
        let day = schedule(&[(6, 0, 9, 0), (17, 0, 22, 0)]);
        // Off 09:00-13:00: exactly the 240-minute threshold
        let short = schedule(&[(6, 0, 9, 0), (13, 0, 22, 0)]);
        let all_off = Schedule::new("Off");
        let cold = outside(-2.0);
        let mild = outside(3.0);
        let no_temperature = Weather::default();
        let late = |h, m| NaiveTime::from_hms_opt(h, m, 30).unwrap();

        // (schedule, now, weather, warm-ups, before_on, after_on, expected start)
        type Case<'a> = (
            &'a Schedule,
            NaiveTime,
            Option<&'a Weather>,
            ColdWarmups,
            u32,
            u32,
            Option<NaiveTime>,
        );
        let cases: Vec<Case> = vec![
            // Gap start + every (12:00), then + 2 * every (15:00)
            (&day, at(12, 0), Some(&cold), on(), 0, 0, Some(at(12, 0))),
            (&day, late(12, 0), Some(&cold), on(), 0, 0, Some(at(12, 0))),
            (&day, at(15, 0), Some(&cold), on(), 0, 0, Some(at(15, 0))),
            // Not at the gap's start, between slots, or a minute late
            (&day, at(9, 0), Some(&cold), on(), 0, 0, None),
            (&day, at(13, 0), Some(&cold), on(), 0, 0, None),
            (&day, at(12, 1), Some(&cold), on(), 0, 0, None),
            // Across midnight: 22:00 + 3 h = 01:00, and 04:00
            (&day, at(1, 0), Some(&cold), on(), 30, 0, Some(at(1, 0))),
            (&day, at(4, 0), Some(&cold), on(), 30, 0, Some(at(4, 0))),
            // 15:00-15:20 would run into a 2-hour early start for 17:00 (from 15:00), but not
            // into one of 100 minutes (from 15:20)
            (&day, at(15, 0), Some(&cold), on(), 120, 0, None),
            (&day, at(15, 0), Some(&cold), on(), 100, 0, Some(at(15, 0))),
            // A late finish of up to 3 h after 09:00 ends just as the 12:00 slot starts: they
            // meet without overlapping. A minute longer and it would overlap.
            (&day, at(12, 0), Some(&cold), on(), 0, 180, Some(at(12, 0))),
            (&day, at(12, 0), Some(&cold), on(), 0, 181, None),
            // The gap exactly at the threshold has none; one minute less and it qualifies
            (&short, at(12, 0), Some(&cold), on(), 0, 0, None),
            (
                &short,
                at(12, 0),
                Some(&cold),
                ColdWarmups {
                    min_gap_minutes: 239,
                    ..on()
                },
                0,
                0,
                Some(at(12, 0)),
            ),
            // Not cold enough, no reading, no temperature, disabled
            (&day, at(12, 0), Some(&mild), on(), 0, 0, None),
            (&day, at(12, 0), None, on(), 0, 0, None),
            (&day, at(12, 0), Some(&no_temperature), on(), 0, 0, None),
            (
                &day,
                at(12, 0),
                Some(&cold),
                ColdWarmups::default(),
                0,
                0,
                None,
            ),
            // During an On period
            (&day, at(7, 0), Some(&cold), on(), 0, 0, None),
            // No On periods: the whole day from midnight
            (&all_off, at(3, 0), Some(&cold), on(), 0, 0, Some(at(3, 0))),
        ];
        for (i, (schedule, now, weather, warmups, before, after, expected)) in
            cases.into_iter().enumerate()
        {
            let result = warm_up(schedule, now, weather, &warmups, before, after);
            assert_eq!(
                result.map(|w| w.started_at),
                expected,
                "case {} at {}",
                i,
                now
            );
        }
    }

    #[test]
    fn test_warm_up_reports_reading_and_end() {
        let day = schedule(&[(6, 0, 9, 0), (17, 0, 22, 0)]);
        let warm = warm_up(&day, at(12, 0), Some(&outside(-4.5)), &on(), 0, 0).unwrap();
        assert_eq!(warm.until, at(12, 20));
        assert_eq!(warm.outside_c, -4.5);
    }

    #[test]
    fn test_continue_warm_up() {
        let day = schedule(&[(6, 0, 9, 0), (17, 0, 22, 0)]);
        let begun = WarmUp {
            started_at: at(23, 50),
            until: at(0, 10),
            outside_c: -2.0,
        };
        let overnight = schedule(&[(6, 0, 9, 0), (17, 0, 22, 0)]);
        assert!(continue_warm_up(&overnight, at(23, 59), &begun).is_some());
        assert!(continue_warm_up(&overnight, at(0, 9), &begun).is_some());
        assert!(continue_warm_up(&overnight, at(0, 10), &begun).is_none());
        // The schedule turned On meanwhile
        let begun = WarmUp {
            started_at: at(12, 0),
            until: at(12, 20),
            outside_c: -2.0,
        };
        let changed = schedule(&[(6, 0, 9, 0), (12, 5, 22, 0)]);
        assert!(continue_warm_up(&day, at(12, 10), &begun).is_some());
        assert!(continue_warm_up(&changed, at(12, 10), &begun).is_none());
    }

    #[test]
    fn test_validate() {
        assert_eq!(ColdWarmups::default().validate(), Ok(()));
        let bad = |w: ColdWarmups| w.validate().unwrap_err();
        assert_eq!(
            bad(ColdWarmups {
                burst_minutes: 180,
                ..on()
            }),
            "cold_warmups.burst_minutes must be less than every_minutes"
        );
        assert_eq!(
            bad(ColdWarmups {
                every_minutes: 0,
                ..on()
            }),
            "cold_warmups.every_minutes must be 1-1440"
        );
        assert_eq!(
            bad(ColdWarmups {
                min_gap_minutes: 2000,
                ..on()
            }),
            "cold_warmups.min_gap_minutes must be 1-1440"
        );
        assert_eq!(
            bad(ColdWarmups {
                burst_minutes: 0,
                ..on()
            }),
            "cold_warmups.burst_minutes must be 1-180"
        );
        assert_eq!(
            bad(ColdWarmups {
                below_c: 25.0,
                ..on()
            }),
            "cold_warmups.below_c must be -30 to 20 °C"
        );
        assert!(
            ColdWarmups {
                below_c: f64::NAN,
                ..on()
            }
            .validate()
            .is_err()
        );
    }
}
