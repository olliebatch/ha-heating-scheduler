use super::Weather;
use crate::schedule::{TARGET_TEMP_RANGE, TimePeriod};
use chrono::NaiveTime;
use serde::{Deserialize, Serialize};

/// How much wind a room's walls and windows catch
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindExposure {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    Cold,
    Wind,
    Sun,
}

/// One part of an adjustment, e.g. +1.0 °C for cold
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reason {
    pub cause: Cause,
    /// °C, rounded to 0.1
    pub delta: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Adjusted {
    pub target: f64,
    /// The scheduled target plus the reasons, before clamping and rounding
    pub raw: f64,
    pub reasons: Vec<Reason>,
}

/// Outside temperature where the cold boost starts, and where it reaches its maximum
const COLD_START: f64 = 5.0;
const COLD_FULL: f64 = -5.0;
const COLD_MAX: f64 = 1.5;
/// Wind speed (km/h) where the wind boost starts, and where it reaches its maximum
const WIND_START: f64 = 20.0;
const WIND_FULL: f64 = 50.0;
/// Cloud cover (%) at or above which a sun window gives no reduction
const SUN_CLOUD_LIMIT: f64 = 30.0;
const SUN_MAX: f64 = 1.5;

/// Fraction of the way from `start` to `full`, clamped to 0..=1
fn ramp(value: f64, start: f64, full: f64) -> f64 {
    ((value - start) / (full - start)).clamp(0.0, 1.0)
}

/// Adjust a scheduled On target for the weather:
/// - cold: from 0 at 5 °C outside up to +1.5 °C at -5 °C;
/// - wind: from 0 at 20 km/h up to the exposure's maximum at 50 km/h
///   (low: none, medium: +0.5 °C, high: +1 °C);
/// - sun: inside one of the zone's sun windows, from -1.5 °C under clear skies to 0 at 30 % cloud.
///
/// Missing weather values give no adjustment for that cause. When anything applies, the result
/// is clamped to 5-30 °C and rounded to 0.5 °C, so small weather changes don't re-command TRVs;
/// otherwise the scheduled target is returned unchanged.
pub fn adjust_target(
    scheduled: f64,
    weather: &Weather,
    sun_windows: &[TimePeriod],
    exposure: WindExposure,
    now: NaiveTime,
) -> Adjusted {
    let mut reasons = Vec::new();
    let mut push = |cause, delta: f64| {
        let delta = (delta * 10.0).round() / 10.0;
        if delta != 0.0 {
            reasons.push(Reason { cause, delta });
        }
    };

    if let Some(outside) = weather.temperature {
        push(Cause::Cold, COLD_MAX * ramp(outside, COLD_START, COLD_FULL));
    }

    let wind_max = match exposure {
        WindExposure::Low => 0.0,
        WindExposure::Medium => 0.5,
        WindExposure::High => 1.0,
    };
    if let Some(wind) = weather.wind_speed {
        push(Cause::Wind, wind_max * ramp(wind, WIND_START, WIND_FULL));
    }

    let in_sun_window = sun_windows.iter().any(|w| w.contains(now));
    if let (true, Some(cloud)) = (in_sun_window, weather.cloud_coverage) {
        push(Cause::Sun, -SUN_MAX * ramp(cloud, SUN_CLOUD_LIMIT, 0.0));
    }

    if reasons.is_empty() {
        return Adjusted {
            target: scheduled,
            raw: scheduled,
            reasons,
        };
    }
    let total: f64 = scheduled + reasons.iter().map(|r| r.delta).sum::<f64>();
    // Keep the sum at one decimal, as the reasons are
    let raw = (total * 10.0).round() / 10.0;
    let clamped = raw.clamp(*TARGET_TEMP_RANGE.start(), *TARGET_TEMP_RANGE.end());
    Adjusted {
        target: (clamped * 2.0).round() / 2.0,
        raw,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weather(temperature: Option<f64>, wind: Option<f64>, cloud: Option<f64>) -> Weather {
        Weather {
            temperature,
            wind_speed: wind,
            cloud_coverage: cloud,
        }
    }

    fn at(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
    }

    fn deltas(adjusted: &Adjusted) -> Vec<(Cause, f64)> {
        adjusted
            .reasons
            .iter()
            .map(|r| (r.cause, r.delta))
            .collect()
    }

    #[test]
    fn test_adjustment_table() {
        use Cause::*;
        use WindExposure::*;
        let morning_sun = vec![TimePeriod::new(8, 0, 11, 0)];
        let none: Vec<TimePeriod> = vec![];

        // (scheduled, weather, sun windows, exposure, time, expected target, expected reasons)
        #[allow(clippy::type_complexity)]
        let cases: Vec<(
            f64,
            Weather,
            &Vec<TimePeriod>,
            WindExposure,
            NaiveTime,
            f64,
            Vec<(Cause, f64)>,
        )> = vec![
            // Mild, calm, cloudy: nothing changes
            (
                21.0,
                weather(Some(12.0), Some(5.0), Some(80.0)),
                &morning_sun,
                Medium,
                at(9, 0),
                21.0,
                vec![],
            ),
            // Unadjusted targets are not rounded
            (
                21.3,
                weather(Some(12.0), Some(5.0), Some(80.0)),
                &none,
                Medium,
                at(9, 0),
                21.3,
                vec![],
            ),
            // Cold ramps from 5 °C to -5 °C
            (
                20.0,
                weather(Some(5.0), None, None),
                &none,
                Medium,
                at(9, 0),
                20.0,
                vec![],
            ),
            (
                20.0,
                weather(Some(0.0), None, None),
                &none,
                Medium,
                at(9, 0),
                21.0,
                vec![(Cold, 0.8)],
            ),
            (
                20.0,
                weather(Some(-5.0), None, None),
                &none,
                Medium,
                at(9, 0),
                21.5,
                vec![(Cold, 1.5)],
            ),
            (
                20.0,
                weather(Some(-20.0), None, None),
                &none,
                Medium,
                at(9, 0),
                21.5,
                vec![(Cold, 1.5)],
            ),
            // Wind depends on exposure
            (
                20.0,
                weather(None, Some(50.0), None),
                &none,
                Low,
                at(9, 0),
                20.0,
                vec![],
            ),
            (
                20.0,
                weather(None, Some(50.0), None),
                &none,
                Medium,
                at(9, 0),
                20.5,
                vec![(Wind, 0.5)],
            ),
            (
                20.0,
                weather(None, Some(80.0), None),
                &none,
                High,
                at(9, 0),
                21.0,
                vec![(Wind, 1.0)],
            ),
            (
                20.0,
                weather(None, Some(35.0), None),
                &none,
                High,
                at(9, 0),
                20.5,
                vec![(Wind, 0.5)],
            ),
            (
                20.0,
                weather(None, Some(20.0), None),
                &none,
                High,
                at(9, 0),
                20.0,
                vec![],
            ),
            // Sun only inside a window and under low cloud
            (
                21.0,
                weather(None, None, Some(0.0)),
                &morning_sun,
                Medium,
                at(9, 0),
                19.5,
                vec![(Sun, -1.5)],
            ),
            (
                21.0,
                weather(None, None, Some(15.0)),
                &morning_sun,
                Medium,
                at(9, 0),
                20.0,
                vec![(Sun, -0.8)],
            ),
            (
                21.0,
                weather(None, None, Some(30.0)),
                &morning_sun,
                Medium,
                at(9, 0),
                21.0,
                vec![],
            ),
            (
                21.0,
                weather(None, None, Some(0.0)),
                &morning_sun,
                Medium,
                at(11, 0),
                21.0,
                vec![],
            ),
            (
                21.0,
                weather(None, None, Some(0.0)),
                &none,
                Medium,
                at(9, 0),
                21.0,
                vec![],
            ),
            // Everything at once: +1.5 cold, +1 wind, -1.5 sun
            (
                21.0,
                weather(Some(-5.0), Some(50.0), Some(0.0)),
                &morning_sun,
                High,
                at(10, 0),
                22.0,
                vec![(Cold, 1.5), (Wind, 1.0), (Sun, -1.5)],
            ),
            // Clamped to 5-30 °C
            (
                29.5,
                weather(Some(-10.0), Some(60.0), None),
                &none,
                High,
                at(9, 0),
                30.0,
                vec![(Cold, 1.5), (Wind, 1.0)],
            ),
            (
                5.5,
                weather(None, None, Some(0.0)),
                &morning_sun,
                Medium,
                at(9, 0),
                5.0,
                vec![(Sun, -1.5)],
            ),
            // Missing weather: no adjustment
            (
                21.0,
                Weather::default(),
                &morning_sun,
                High,
                at(9, 0),
                21.0,
                vec![],
            ),
        ];

        for (i, (scheduled, weather, windows, exposure, now, target, reasons)) in
            cases.into_iter().enumerate()
        {
            let adjusted = adjust_target(scheduled, &weather, windows, exposure, now);
            assert_eq!(adjusted.target, target, "case {}: {:?}", i, adjusted);
            assert_eq!(deltas(&adjusted), reasons, "case {}", i);
        }
    }

    #[test]
    fn test_small_weather_changes_dont_change_target() {
        // Rounding to 0.5 °C keeps the target steady as the temperature drifts
        let targets: Vec<f64> = [1.0, 1.2, 1.4, 1.6]
            .iter()
            .map(|t| {
                adjust_target(
                    20.0,
                    &weather(Some(*t), None, None),
                    &[],
                    WindExposure::Medium,
                    at(9, 0),
                )
                .target
            })
            .collect();
        assert_eq!(targets, vec![20.5, 20.5, 20.5, 20.5]);
    }

    #[test]
    fn test_sun_window_across_midnight() {
        let windows = [TimePeriod::new(23, 0, 1, 0)];
        let clear = weather(None, None, Some(0.0));
        let adjusted = adjust_target(20.0, &clear, &windows, WindExposure::Medium, at(0, 30));
        assert_eq!(adjusted.target, 18.5);
    }

    #[test]
    fn test_raw_shows_the_unrounded_sum() {
        let cold = |t| weather(Some(t), None, None);
        let a = adjust_target(20.0, &cold(3.4), &[], WindExposure::Medium, at(9, 0));
        assert_eq!((a.raw, a.target), (20.2, 20.0));
        let a = adjust_target(20.0, &cold(3.3), &[], WindExposure::Medium, at(9, 0));
        assert_eq!((a.raw, a.target), (20.3, 20.5));
        let a = adjust_target(29.5, &cold(-10.0), &[], WindExposure::Medium, at(9, 0));
        assert_eq!((a.raw, a.target), (31.0, 30.0));
    }
}
