use super::Weather;
use super::adjust::{Cause, WindExposure};
use super::early::{lead_minutes, seconds_until};
use crate::schedule::{HeatingState, Schedule, ScheduleEntry};
use chrono::NaiveTime;
use serde::{Deserialize, Serialize};

/// An On period kept going past its end
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LateFinish {
    /// When the On period was scheduled to end
    pub period_end: NaiveTime,
    /// How long past the end it keeps heating, decided when it ended
    pub extra_minutes: u32,
    pub causes: Vec<Cause>,
}

/// How soon after a period's end a late finish can begin, in seconds: a scheduler tick (15 s)
/// with room to spare. It's decided at the end, so a day that turns cold later in an Off gap
/// doesn't switch the heating back on.
const DECIDE_WITHIN: u32 = 60;

fn on_at(schedule: &Schedule, now: NaiveTime) -> bool {
    schedule
        .entries
        .iter()
        .any(|e| e.heating_state == HeatingState::On && e.time_period.contains(now))
}

/// If an On period has just ended (within [`DECIDE_WITHIN`]) and the schedule is now Off, that
/// period and how long to keep it going: the same scale as an early start, from none at 10 °C or
/// warmer to all of `max_minutes` at -2 °C or colder, with wind adding within the maximum.
/// Back-to-back On periods need nothing, because the schedule isn't Off between them.
pub fn late_finish<'a>(
    schedule: &'a Schedule,
    now: NaiveTime,
    weather: &Weather,
    exposure: WindExposure,
    max_minutes: u32,
) -> Option<(&'a ScheduleEntry, LateFinish)> {
    if max_minutes == 0 || on_at(schedule, now) {
        return None;
    }
    let (extra, causes) = lead_minutes(weather, exposure, max_minutes);
    if extra == 0 {
        return None;
    }
    // The On period that ended most recently, i.e. the one this Off follows
    let ended = schedule
        .entries
        .iter()
        .filter(|e| e.heating_state == HeatingState::On)
        .min_by_key(|e| seconds_until(e.time_period.end, now))?;
    (seconds_until(ended.time_period.end, now) < DECIDE_WITHIN).then_some((
        ended,
        LateFinish {
            period_end: ended.time_period.end,
            extra_minutes: extra,
            causes,
        },
    ))
}

/// Keep a late finish that has begun going for the minutes decided at the end, whatever the
/// weather does meanwhile. Returns the ended period's entry while the schedule is still Off and
/// within the extra minutes; None once they're up, or if the schedule changed.
pub fn continue_late_finish<'a>(
    schedule: &'a Schedule,
    now: NaiveTime,
    begun: &LateFinish,
) -> Option<&'a ScheduleEntry> {
    if on_at(schedule, now) {
        return None;
    }
    let entry = schedule
        .entries
        .iter()
        .find(|e| e.heating_state == HeatingState::On && e.time_period.end == begun.period_end)?;
    (seconds_until(entry.time_period.end, now) < begun.extra_minutes * 60).then_some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::TimePeriod;

    fn at(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
    }

    fn outside(temperature: f64, wind: f64) -> Weather {
        Weather {
            temperature: Some(temperature),
            wind_speed: Some(wind),
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

    #[test]
    fn test_late_finish_table() {
        let morning = schedule(&[(7, 0, 9, 0)]);
        let overnight = schedule(&[(22, 0, 0, 0)]);
        let late_night = schedule(&[(23, 0, 23, 59)]);
        let back_to_back = schedule(&[(6, 0, 7, 0), (7, 0, 9, 0)]);
        let off = Schedule::new("Off");
        let cold = outside(-2.0, 0.0); // the full maximum
        let mild = outside(4.0, 0.0); // half
        let warm = outside(12.0, 0.0);
        let just = |h, m| NaiveTime::from_hms_opt(h, m, 30).unwrap();

        // (schedule, now, weather, max, expected (period end, extra minutes))
        type Case<'a> = (
            &'a Schedule,
            NaiveTime,
            &'a Weather,
            u32,
            Option<(NaiveTime, u32)>,
        );
        let cases: Vec<Case> = vec![
            (&morning, at(9, 0), &cold, 30, Some((at(9, 0), 30))),
            (&morning, just(9, 0), &mild, 30, Some((at(9, 0), 15))),
            // Decided at the end: a minute later is too late to begin
            (&morning, at(9, 1), &cold, 30, None),
            (&morning, at(8, 59), &cold, 30, None),
            // Ending at midnight, and just before it
            (&overnight, at(0, 0), &cold, 30, Some((at(0, 0), 30))),
            (&late_night, just(23, 59), &cold, 20, Some((at(23, 59), 20))),
            // Back-to-back On periods: the schedule is still On at 07:00
            (&back_to_back, at(7, 0), &cold, 30, None),
            (&back_to_back, at(9, 0), &cold, 30, Some((at(9, 0), 30))),
            // Disabled, warm weather, no On periods
            (&morning, at(9, 0), &cold, 0, None),
            (&morning, at(9, 0), &warm, 30, None),
            (&off, at(9, 0), &cold, 30, None),
        ];
        for (i, (schedule, now, weather, max, expected)) in cases.into_iter().enumerate() {
            let result = late_finish(schedule, now, weather, WindExposure::Medium, max);
            assert_eq!(
                result.map(|(_, l)| (l.period_end, l.extra_minutes)),
                expected,
                "case {} at {}",
                i,
                now
            );
        }
    }

    #[test]
    fn test_late_finish_reports_entry_and_causes() {
        let morning = schedule(&[(7, 0, 9, 0)]);
        let (entry, late) = late_finish(
            &morning,
            at(9, 0),
            &outside(15.0, 50.0),
            WindExposure::High,
            40,
        )
        .unwrap();
        assert_eq!(entry.target_temp, Some(21.0));
        assert_eq!(late.extra_minutes, 20);
        assert_eq!(late.causes, vec![Cause::Wind]);
    }

    #[test]
    fn test_continue_late_finish() {
        let morning = schedule(&[(7, 0, 9, 0)]);
        let begun = LateFinish {
            period_end: at(9, 0),
            extra_minutes: 20,
            causes: vec![Cause::Cold],
        };
        assert!(continue_late_finish(&morning, at(9, 0), &begun).is_some());
        assert!(continue_late_finish(&morning, at(9, 19), &begun).is_some());
        // The extra minutes are up, or it's the next day's period
        assert!(continue_late_finish(&morning, at(9, 20), &begun).is_none());
        assert!(continue_late_finish(&morning, at(7, 30), &begun).is_none());
        // The schedule changed so there's no period ending at 09:00 any more
        let later = schedule(&[(7, 0, 10, 0)]);
        assert!(continue_late_finish(&later, at(9, 10), &begun).is_none());

        // Across midnight
        let overnight = schedule(&[(22, 0, 0, 0)]);
        let begun = LateFinish {
            period_end: at(0, 0),
            ..begun
        };
        assert!(continue_late_finish(&overnight, at(0, 15), &begun).is_some());
        assert!(continue_late_finish(&overnight, at(0, 20), &begun).is_none());
    }
}
