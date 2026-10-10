use super::Weather;
use super::adjust::{Cause, WindExposure};
use crate::schedule::{HeatingState, Schedule, ScheduleEntry};
use chrono::{NaiveTime, Timelike};
use serde::{Deserialize, Serialize};

/// An On period being started early
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EarlyStart {
    /// When the On period is scheduled to start
    pub period_start: NaiveTime,
    /// How early the weather says to start it
    pub lead_minutes: u32,
    /// How many minutes before the period heating actually started: less than `lead_minutes`
    /// when the period was added, or the weather turned cold, within the lead
    pub minutes_early: u32,
    pub causes: Vec<Cause>,
}

/// Outside temperature (°C) below which early starts begin, and where they reach the maximum
const COLD_START: f64 = 10.0;
const COLD_FULL: f64 = -2.0;
/// Wind speed (km/h) where wind starts adding lead time, and where it adds its most
const WIND_START: f64 = 20.0;
const WIND_FULL: f64 = 50.0;

fn ramp(value: f64, start: f64, full: f64) -> f64 {
    ((value - start) / (full - start)).clamp(0.0, 1.0)
}

/// How many minutes early to start, out of `max_minutes`: from none at 10 °C or warmer up to
/// all of it at -2 °C or colder. Wind from 20 km/h (full at 50 km/h) adds up to a quarter of
/// the maximum for medium exposure and half for high (none for low), within the maximum.
pub fn lead_minutes(
    weather: &Weather,
    exposure: WindExposure,
    max_minutes: u32,
) -> (u32, Vec<Cause>) {
    let cold = weather
        .temperature
        .map_or(0.0, |t| ramp(t, COLD_START, COLD_FULL));
    let wind_weight = match exposure {
        WindExposure::Low => 0.0,
        WindExposure::Medium => 0.25,
        WindExposure::High => 0.5,
    };
    let wind = weather
        .wind_speed
        .map_or(0.0, |w| wind_weight * ramp(w, WIND_START, WIND_FULL));

    let minutes = (f64::from(max_minutes) * (cold + wind).min(1.0)).round() as u32;
    let mut causes = Vec::new();
    if minutes > 0 {
        if cold > 0.0 {
            causes.push(Cause::Cold);
        }
        if wind > 0.0 {
            causes.push(Cause::Wind);
        }
    }
    (minutes, causes)
}

/// Seconds from `now` until `start`, wrapping past midnight (0 when they're equal)
fn seconds_until(now: NaiveTime, start: NaiveTime) -> u32 {
    const DAY: u32 = 24 * 60 * 60;
    let (now, start) = (
        now.num_seconds_from_midnight(),
        start.num_seconds_from_midnight(),
    );
    (start + DAY - now) % DAY
}

/// If the schedule is Off at `now` and its next On period starts within the weather's lead time,
/// that period and how early it is being started. End times never move.
pub fn early_start<'a>(
    schedule: &'a Schedule,
    now: NaiveTime,
    weather: &Weather,
    exposure: WindExposure,
    max_minutes: u32,
) -> Option<(&'a ScheduleEntry, EarlyStart)> {
    if max_minutes == 0 {
        return None;
    }
    let currently_on = schedule
        .entries
        .iter()
        .any(|e| e.heating_state == HeatingState::On && e.time_period.contains(now));
    if currently_on {
        return None;
    }
    let (lead, causes) = lead_minutes(weather, exposure, max_minutes);
    if lead == 0 {
        return None;
    }

    let next_on = schedule
        .entries
        .iter()
        .filter(|e| e.heating_state == HeatingState::On)
        .min_by_key(|e| seconds_until(now, e.time_period.start))?;
    let until = seconds_until(now, next_on.time_period.start);
    (until <= lead * 60).then_some((
        next_on,
        EarlyStart {
            period_start: next_on.time_period.start,
            lead_minutes: lead,
            minutes_early: (until + 30) / 60,
            causes,
        },
    ))
}

/// Keep an early start that has begun going until its period starts, whatever the weather does
/// meanwhile, so a warming reading can't turn the zone Off and On again. Returns the period's
/// entry while the schedule is still Off and that period (same start) is still ahead within the
/// lead; None once it starts or if the schedule changed.
pub fn continue_early_start<'a>(
    schedule: &'a Schedule,
    now: NaiveTime,
    begun: &EarlyStart,
) -> Option<&'a ScheduleEntry> {
    let currently_on = schedule
        .entries
        .iter()
        .any(|e| e.heating_state == HeatingState::On && e.time_period.contains(now));
    if currently_on {
        return None;
    }
    let entry = schedule.entries.iter().find(|e| {
        e.heating_state == HeatingState::On && e.time_period.start == begun.period_start
    })?;
    let until = seconds_until(now, entry.time_period.start);
    (until > 0 && until <= begun.lead_minutes * 60).then_some(entry)
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
    fn test_lead_minutes_table() {
        use Cause::*;
        use WindExposure::*;
        let cases: Vec<(Weather, WindExposure, u32, u32, Vec<Cause>)> = vec![
            (outside(10.0, 0.0), Medium, 30, 0, vec![]),
            (outside(15.0, 60.0), Low, 30, 0, vec![]),
            (outside(4.0, 0.0), Medium, 30, 15, vec![Cold]),
            (outside(-2.0, 0.0), Medium, 30, 30, vec![Cold]),
            (outside(-10.0, 0.0), Medium, 60, 60, vec![Cold]),
            // Wind adds within the cap
            (outside(15.0, 50.0), High, 30, 15, vec![Wind]),
            (outside(15.0, 50.0), Medium, 40, 10, vec![Wind]),
            (outside(4.0, 50.0), High, 30, 30, vec![Cold, Wind]),
            (outside(-5.0, 80.0), High, 30, 30, vec![Cold, Wind]),
            // Disabled, and no weather values
            (outside(-5.0, 80.0), High, 0, 0, vec![]),
            (Weather::default(), High, 30, 0, vec![]),
        ];
        for (i, (weather, exposure, max, minutes, causes)) in cases.into_iter().enumerate() {
            assert_eq!(
                lead_minutes(&weather, exposure, max),
                (minutes, causes),
                "case {}",
                i
            );
        }
    }

    #[test]
    fn test_early_start_table() {
        let morning = schedule(&[(7, 0, 9, 0)]);
        let overnight = schedule(&[(23, 30, 1, 0)]);
        // Two On periods with a short Off gap between them: 06:00-07:00 and 07:20-09:00
        let back_to_back = schedule(&[(6, 0, 7, 0), (7, 20, 9, 0)]);
        let cold = outside(-2.0, 0.0); // full 30 min lead
        let mild = outside(4.0, 0.0); // 15 min lead
        let warm = outside(12.0, 0.0);
        let after_midnight = schedule(&[(0, 10, 6, 0)]);
        let off = Schedule::new("Off");

        // (schedule, now, weather, max, expected period start)
        let cases: Vec<(&Schedule, NaiveTime, &Weather, u32, Option<NaiveTime>)> = vec![
            (&morning, at(6, 30), &cold, 30, Some(at(7, 0))),
            (&morning, at(6, 29), &cold, 30, None),
            (&morning, at(6, 50), &mild, 30, Some(at(7, 0))),
            (&morning, at(6, 40), &mild, 30, None),
            // Already On: nothing to start early; after the period ends, nothing until tomorrow
            (&morning, at(7, 30), &cold, 30, None),
            (&morning, at(9, 0), &cold, 30, None),
            // Across midnight, both ways
            (&overnight, at(23, 5), &cold, 30, Some(at(23, 30))),
            (&after_midnight, at(23, 50), &cold, 30, Some(at(0, 10))),
            // Back-to-back: only the Off gap is filled, and only within the lead
            (&back_to_back, at(7, 5), &cold, 30, Some(at(7, 20))),
            (&back_to_back, at(6, 30), &cold, 30, None),
            // Disabled, warm weather, no On periods
            (&morning, at(6, 45), &cold, 0, None),
            (&morning, at(6, 45), &warm, 30, None),
            (&off, at(6, 45), &cold, 30, None),
        ];
        for (i, (schedule, now, weather, max, expected)) in cases.into_iter().enumerate() {
            let result = early_start(schedule, now, weather, WindExposure::Medium, max);
            assert_eq!(
                result.map(|(_, e)| e.period_start),
                expected,
                "case {} at {}",
                i,
                now
            );
        }
    }

    #[test]
    fn test_early_start_reports_lead_and_entry() {
        let morning = schedule(&[(7, 0, 9, 0)]);
        let (entry, early) = early_start(
            &morning,
            at(6, 50),
            &outside(4.0, 0.0),
            WindExposure::Medium,
            30,
        )
        .unwrap();
        assert_eq!(entry.target_temp, Some(21.0));
        assert_eq!(early.lead_minutes, 15);
        assert_eq!(early.minutes_early, 10);
        assert_eq!(early.causes, vec![Cause::Cold]);
    }

    #[test]
    fn test_early_start_added_within_the_lead() {
        // A period added 10 minutes before it starts, on a day calling for 30 minutes' lead
        let late = schedule(&[(12, 4, 13, 0)]);
        let (_, early) = early_start(
            &late,
            at(11, 54),
            &outside(-2.0, 0.0),
            WindExposure::Medium,
            30,
        )
        .unwrap();
        assert_eq!(early.lead_minutes, 30);
        assert_eq!(early.minutes_early, 10);

        // Across midnight, and rounded to the nearest minute
        let after_midnight = schedule(&[(0, 5, 6, 0)]);
        let now = NaiveTime::from_hms_opt(23, 52, 40).unwrap();
        let (_, early) = early_start(
            &after_midnight,
            now,
            &outside(-2.0, 0.0),
            WindExposure::Medium,
            30,
        )
        .unwrap();
        assert_eq!(early.minutes_early, 12);
    }

    #[test]
    fn test_continue_early_start() {
        let morning = schedule(&[(7, 0, 9, 0)]);
        let begun = EarlyStart {
            period_start: at(7, 0),
            lead_minutes: 30,
            minutes_early: 30,
            causes: vec![Cause::Cold],
        };

        assert!(continue_early_start(&morning, at(6, 41), &begun).is_some());
        assert!(continue_early_start(&morning, at(6, 59), &begun).is_some());
        // Once the period starts the schedule takes over; outside the lead it's over
        assert!(continue_early_start(&morning, at(7, 0), &begun).is_none());
        assert!(continue_early_start(&morning, at(6, 20), &begun).is_none());
        // The schedule changed so there's no 07:00 period any more
        let later = schedule(&[(8, 0, 9, 0)]);
        assert!(continue_early_start(&later, at(6, 45), &begun).is_none());
    }
}
