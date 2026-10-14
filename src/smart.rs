//! The smart decision: when to start heating for a zone's next comfort time (one of its schedule's
//! On periods), whether it needs heating at all, and every number that went into it. A pure
//! function of its inputs; shadow and smart zones get one each tick (see `shadow`), and nothing is
//! sent from it yet.

use crate::schedule::{HeatingState, Schedule, TimePeriod};
use crate::weather::adjust::WindExposure;
use crate::weather::{ForecastHour, Weather};
use crate::zones::room::{Facing, Room};
use chrono::{DateTime, Local, NaiveTime, TimeDelta, Timelike};
use serde::{Deserialize, Serialize};

/// One of the schedule's On periods: the room should be at `target` from `start` to `end`
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ComfortTime {
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub target: f64,
}

/// The On period covering `now` (and true), else the next one to start (and false). None when
/// the schedule has no On period. A period without a target uses `default_target`.
pub fn comfort_time(
    schedule: &Schedule,
    now: NaiveTime,
    default_target: f64,
) -> Option<(ComfortTime, bool)> {
    let on = schedule
        .entries
        .iter()
        .filter(|e| e.heating_state == HeatingState::On);
    let comfort = |e: &crate::schedule::ScheduleEntry| ComfortTime {
        start: e.time_period.start,
        end: e.time_period.end,
        target: e.target_temp.unwrap_or(default_target),
    };
    if let Some(current) = on.clone().find(|e| e.time_period.contains(now)) {
        return Some((comfort(current), true));
    }
    on.min_by_key(|e| seconds_until(now, e.time_period.start))
        .map(|e| (comfort(e), false))
}

/// Seconds from `now` until `then`, wrapping past midnight (0 when they're equal)
fn seconds_until(now: NaiveTime, then: NaiveTime) -> i64 {
    const DAY: i64 = 24 * 60 * 60;
    let (now, then) = (
        i64::from(now.num_seconds_from_midnight()),
        i64::from(then.num_seconds_from_midnight()),
    );
    (then - now).rem_euclid(DAY)
}

/// Everything the decision looks at
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    pub now: DateTime<Local>,
    /// The comfort time now (`inside`) or next
    pub comfort: ComfortTime,
    pub inside: bool,
    /// The room temperature now (°C), if it's known
    pub room_c: Option<f64>,
    /// The weather now, if it's known
    pub weather: Option<&'a Weather>,
    /// The hourly forecast, soonest first (may be empty)
    pub forecast: &'a [ForecastHour],
    pub room: &'a Room,
    pub wind_exposure: WindExposure,
    /// The zone's sun windows, which say it's sunny whatever way the room faces
    pub sun_windows: &'a [TimePeriod],
    /// Today's sunrise and sunset. Sun only counts between them; without them, the way a room faces
    /// brings no sun, and only sun windows do.
    pub daylight: Option<TimePeriod>,
    /// The most minutes heating may start before a comfort time
    pub max_early_minutes: u32,
    /// The zone's decision on the last tick, if there was one, so a decision isn't undone by a
    /// small change: heating that has begun for a comfort time keeps going until it starts, and
    /// once heating isn't needed it stays so until the room is [`HYSTERESIS_C`] below the target
    pub previous: Option<&'a Decision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Heat to the decision's target
    Heat,
    /// Nothing to do: before a comfort time's start, or between comfort times
    Off,
    /// A comfort time is due or under way, but the room is warm and mild weather will keep it so
    NotNeeded,
}

/// One number that went into a decision, for the "why"
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Factor {
    /// The outside temperature at the comfort time's start: from the forecast, or else now
    Outside { c: f64, forecast: bool },
    /// The wind takes this much off the outside temperature for this room. `faces_hit` are the
    /// room's sides the wind blows onto (empty when the direction isn't known or used).
    Wind {
        chill_c: f64,
        speed_kmh: f64,
        bearing: Option<f64>,
        faces_hit: Vec<Facing>,
    },
    /// The sun adds this much
    Sun { gain_c: f64 },
    /// The outside temperature after wind and sun, as this room feels it
    FeelsLike { c: f64 },
    /// The room now
    Indoors { c: f64 },
    /// How fast the room is expected to warm in this weather
    WarmRate { c_per_hour: f64 },
    /// How long before the start heating needs to begin; `capped` when the zone's maximum
    /// early start cut it short
    Lead { minutes: u32, capped: bool },
    /// Between comfort times, kept at the setback temperature
    Setback { c: f64 },
    /// No weather reading: heating starts at the comfort time's start, as scheduled
    NoWeather,
    /// No room temperature: the schedule decides, as in Schedule mode
    NoRoomTemperature,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub action: Action,
    /// The temperature to heat to (Heat only)
    pub target: Option<f64>,
    /// When heating starts (or started) for the comfort time
    pub start_at: NaiveTime,
    pub comfort: ComfortTime,
    pub reasons: Vec<Factor>,
}

/// The most minutes a smart decision may start heating before a comfort time
pub const SMART_MAX_EARLY_MINUTES: u32 = 120;
/// How far (°C) below the target a room that didn't need heating may drop before it does
pub const HYSTERESIS_C: f64 = 0.5;
/// Minutes added to every lead, so the room is there by the start rather than just reaching it
pub const MARGIN_MINUTES: u32 = 10;
/// °C taken off per km/h of wind square onto the room
fn chill_per_kmh(exposure: WindExposure) -> f64 {
    match exposure {
        WindExposure::Low => 0.02,
        WindExposure::Medium => 0.05,
        WindExposure::High => 0.08,
    }
}
/// Share of the wind chill a room gets when the wind blows onto none of its sides
const LEEWARD: f64 = 0.3;
/// °C full sun adds at the highest sun gain, under a clear sky
const FULL_SUN_C: f64 = 4.0;
/// How far below the target (°C) it can feel outside for a warm room to stay warm on its own
const MILD_WITHIN_C: f64 = 5.0;
/// The difference between the target and outside (°C) at which a room with heat loss 1 warms at
/// half its rate
const HALF_RATE_GAP_C: f64 = 10.0;

fn degrees(face: Facing) -> f64 {
    match face {
        Facing::N => 0.0,
        Facing::E => 90.0,
        Facing::S => 180.0,
        Facing::W => 270.0,
    }
}

/// The sides of a house the sun is on at `hour`, while it's up: east in the morning, south at midday and west
/// in the afternoon
fn sunny_faces(hour: u32) -> Vec<Facing> {
    [(Facing::E, 6..11), (Facing::S, 10..15), (Facing::W, 14..19)]
        .into_iter()
        .filter(|(_, hours)| hours.contains(&hour))
        .map(|(face, _)| face)
        .collect()
}

/// The decision for one zone right now
pub fn decide(inputs: &Inputs) -> Decision {
    let comfort = inputs.comfort;
    let until_start = seconds_until(inputs.now.time(), comfort.start);
    let starts = inputs.now + TimeDelta::seconds(until_start);
    let scheduled = |action, reasons| Decision {
        action,
        target: (action == Action::Heat).then_some(comfort.target),
        start_at: comfort.start,
        comfort,
        reasons,
    };
    let between = |start_at: NaiveTime, mut reasons: Vec<Factor>| match inputs.room.setback_c {
        Some(c) => {
            reasons.push(Factor::Setback { c });
            Decision {
                action: Action::Heat,
                target: Some(c),
                start_at,
                comfort,
                reasons,
            }
        }
        None => Decision {
            action: Action::Off,
            target: None,
            start_at,
            comfort,
            reasons,
        },
    };

    // Without the room temperature there's nothing to go on: do what the schedule does
    let Some(room_c) = inputs.room_c else {
        let reasons = vec![Factor::NoRoomTemperature];
        return if inputs.inside {
            scheduled(Action::Heat, reasons)
        } else {
            between(comfort.start, reasons)
        };
    };
    let mut reasons = Vec::new();

    // How it'll feel outside to this room at the start (now, once it's under way)
    let at = if inputs.inside { inputs.now } else { starts };
    let hour = inputs
        .forecast
        .iter()
        .find(|h| h.at <= at && at < h.at + TimeDelta::hours(1));
    let outside = match (hour.map(|h| &h.weather), inputs.weather) {
        (
            Some(
                w @ Weather {
                    temperature: Some(_),
                    ..
                },
            ),
            _,
        ) => Some((w, true)),
        (
            _,
            Some(
                w @ Weather {
                    temperature: Some(_),
                    ..
                },
            ),
        ) => Some((w, false)),
        _ => None,
    };
    let Some((weather, from_forecast)) = outside else {
        reasons.push(Factor::Indoors { c: room_c });
        reasons.push(Factor::NoWeather);
        return if inputs.inside {
            scheduled(Action::Heat, reasons)
        } else {
            between(comfort.start, reasons)
        };
    };
    let outside_c = round1(weather.temperature.unwrap_or_default());
    reasons.push(Factor::Outside {
        c: outside_c,
        forecast: from_forecast,
    });

    let mut feels_like = outside_c;
    if let Some(speed) = weather.wind_speed.filter(|s| *s > 0.0) {
        let per_kmh = chill_per_kmh(inputs.wind_exposure);
        let (share, faces_hit) = match (weather.wind_bearing, inputs.room.faces.is_empty()) {
            (Some(bearing), false) => {
                let hit: Vec<Facing> = inputs
                    .room
                    .faces
                    .iter()
                    .copied()
                    .filter(|f| (bearing - degrees(*f)).to_radians().cos() > 0.0)
                    .collect();
                let squareness = inputs
                    .room
                    .faces
                    .iter()
                    .map(|f| (bearing - degrees(*f)).to_radians().cos().max(0.0))
                    .fold(0.0, f64::max);
                (LEEWARD + (1.0 - LEEWARD) * squareness, hit)
            }
            _ => (1.0, Vec::new()),
        };
        let chill_c = round1(speed * per_kmh * share);
        feels_like -= chill_c;
        reasons.push(Factor::Wind {
            chill_c,
            speed_kmh: round1(speed),
            bearing: weather.wind_bearing.map(f64::round),
            faces_hit,
        });
    }

    let local = at.with_timezone(&Local).time();
    // Sun windows are the user's word for when the sun's on the room, but not after dark
    let daylight = inputs.daylight.map(|d| d.contains(local));
    let sunny = (daylight != Some(false) && inputs.sun_windows.iter().any(|w| w.contains(local)))
        || (daylight == Some(true)
            && sunny_faces(local.hour())
                .iter()
                .any(|f| inputs.room.faces.contains(f)));
    if let (true, Some(cloud)) = (sunny, weather.cloud_coverage) {
        let gain_c = round1(inputs.room.sun_gain * FULL_SUN_C * (1.0 - cloud / 100.0));
        if gain_c > 0.0 {
            feels_like += gain_c;
            reasons.push(Factor::Sun { gain_c });
        }
    }
    let feels_like = round1(feels_like);
    reasons.push(Factor::FeelsLike { c: feels_like });
    reasons.push(Factor::Indoors { c: room_c });

    // Warm and mild enough to stay warm: no need to heat for this comfort time yet. Once that's
    // decided it holds until the room is clearly below the target, so it doesn't flip each tick.
    let same = |action| {
        inputs
            .previous
            .is_some_and(|p| p.action == action && p.comfort == comfort)
    };
    let warm_from = if same(Action::NotNeeded) {
        comfort.target - HYSTERESIS_C
    } else {
        comfort.target
    };
    let warm = room_c >= warm_from && feels_like >= comfort.target - MILD_WITHIN_C;

    if inputs.inside {
        return scheduled(
            if warm {
                Action::NotNeeded
            } else {
                Action::Heat
            },
            reasons,
        );
    }

    let gap = (comfort.target - feels_like).max(0.0);
    let rate =
        inputs.room.warm_rate_c_per_hour / (1.0 + inputs.room.heat_loss * gap / HALF_RATE_GAP_C);
    let rate = round1(rate).max(0.1);
    reasons.push(Factor::WarmRate { c_per_hour: rate });
    let needed = if room_c < comfort.target {
        ((comfort.target - room_c) / rate * 60.0 - 1e-6).ceil() as u32 + MARGIN_MINUTES
    } else {
        0
    };
    let lead = needed.min(inputs.max_early_minutes);
    reasons.push(Factor::Lead {
        minutes: lead,
        capped: lead < needed,
    });
    let start_at = comfort.start - TimeDelta::minutes(i64::from(lead));

    // Heating that has begun for this comfort time keeps going, from when it began, even though
    // the lead shrinks as the room warms
    // (Only a Heat decided before the start counts: one with a lead, and not the setback. The
    // comfort time's own Heat, once it's over, must not carry on to the next one.)
    let begun = inputs.previous.filter(|p| {
        same(Action::Heat)
            && p.reasons.iter().any(|r| matches!(r, Factor::Lead { .. }))
            && !p
                .reasons
                .iter()
                .any(|r| matches!(r, Factor::Setback { .. }))
    });
    // (A warm room needs no lead, so it waits for the comfort time to start)
    if let Some(begun) = begun {
        Decision {
            action: Action::Heat,
            target: Some(comfort.target),
            start_at: begun.start_at,
            comfort,
            reasons,
        }
    } else if lead > 0 && until_start <= i64::from(lead) * 60 {
        Decision {
            action: Action::Heat,
            target: Some(comfort.target),
            start_at,
            comfort,
            reasons,
        }
    } else {
        between(start_at, reasons)
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use Factor::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 1, 15, h, m, 0).unwrap()
    }
    fn time(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }
    fn weather(c: f64, wind: f64, bearing: Option<f64>, cloud: f64) -> Weather {
        Weather {
            temperature: Some(c),
            wind_speed: Some(wind),
            wind_bearing: bearing,
            cloud_coverage: Some(cloud),
        }
    }

    /// 17:00-22:00 at 20 °C, up to two hours early
    struct Case {
        now: DateTime<Local>,
        inside: bool,
        room_c: Option<f64>,
        weather: Option<Weather>,
        forecast: Vec<ForecastHour>,
        room: Room,
        exposure: WindExposure,
        daylight: Option<TimePeriod>,
        previous: Option<Decision>,
    }

    impl Case {
        fn new(now: DateTime<Local>, room_c: f64, weather: Weather) -> Case {
            Case {
                now,
                inside: false,
                room_c: Some(room_c),
                weather: Some(weather),
                forecast: Vec::new(),
                room: Room::default(),
                exposure: WindExposure::Medium,
                // A January day
                daylight: Some(TimePeriod::new(8, 0, 16, 0)),
                previous: None,
            }
        }
        fn decide(&self, comfort: ComfortTime) -> Decision {
            decide(&Inputs {
                now: self.now,
                comfort,
                inside: self.inside,
                room_c: self.room_c,
                weather: self.weather.as_ref(),
                forecast: &self.forecast,
                room: &self.room,
                wind_exposure: self.exposure,
                sun_windows: &[],
                daylight: self.daylight,
                max_early_minutes: 120,
                previous: self.previous.as_ref(),
            })
        }
        fn evening(&self) -> Decision {
            self.decide(ComfortTime {
                start: time(17, 0),
                end: time(22, 0),
                target: 20.0,
            })
        }
    }

    #[test]
    fn test_cold_wind_onto_a_north_room_starts_earlier_than_onto_its_back() {
        let mut north_wind = Case::new(at(15, 0), 19.0, weather(3.0, 40.0, Some(0.0), 100.0));
        north_wind.room.faces = vec![Facing::N];
        north_wind.exposure = WindExposure::High;
        let d = north_wind.evening();
        assert_eq!(
            d.reasons,
            vec![
                Outside {
                    c: 3.0,
                    forecast: false
                },
                Wind {
                    chill_c: 3.2,
                    speed_kmh: 40.0,
                    bearing: Some(0.0),
                    faces_hit: vec![Facing::N]
                },
                FeelsLike { c: -0.2 },
                Indoors { c: 19.0 },
                // 1.5 °C/h, halved at heat loss 1 for every 10 °C below the target
                WarmRate { c_per_hour: 0.7 },
                // 1 °C at 0.7 °C/h is 86 minutes, plus the margin
                Lead {
                    minutes: 96,
                    capped: false
                },
            ]
        );
        assert_eq!((d.action, d.start_at), (Action::Off, time(15, 24)));

        // The same wind on the room's back: under a third of the chill
        let mut south_wind = north_wind;
        south_wind.weather = Some(weather(3.0, 40.0, Some(180.0), 100.0));
        let d = south_wind.evening();
        assert!(d.reasons.contains(&Wind {
            chill_c: 1.0,
            speed_kmh: 40.0,
            bearing: Some(180.0),
            faces_hit: vec![]
        }));
        assert!(d.reasons.contains(&Lead {
            minutes: 85,
            capped: false
        }));
        assert_eq!(d.start_at, time(15, 35));

        // Once the lead's reached, heat
        south_wind.now = at(15, 35);
        let d = south_wind.evening();
        assert_eq!((d.action, d.target), (Action::Heat, Some(20.0)));
    }

    #[test]
    fn test_a_long_lead_is_capped_at_the_zones_maximum() {
        let case = Case::new(at(15, 0), 14.0, weather(-5.0, 0.0, None, 100.0));
        let d = case.evening();
        // 6 °C at 1.0 °C/h is six hours, but two is the most
        assert!(
            d.reasons.contains(&Lead {
                minutes: 120,
                capped: true
            }),
            "{:?}",
            d.reasons
        );
        assert_eq!((d.action, d.start_at), (Action::Heat, time(15, 0)));
    }

    #[test]
    fn test_sunny_and_mild_on_a_south_room_is_not_needed() {
        let mut case = Case::new(at(12, 0), 20.5, weather(15.0, 0.0, None, 0.0));
        case.room.faces = vec![Facing::S];
        let afternoon = ComfortTime {
            start: time(13, 0),
            end: time(18, 0),
            target: 20.0,
        };

        // Already warm: no lead, so nothing happens before the start
        let d = case.decide(afternoon);
        assert!(d.reasons.contains(&Sun { gain_c: 2.0 }));
        assert!(d.reasons.contains(&FeelsLike { c: 17.0 }));
        assert_eq!((d.action, d.start_at), (Action::Off, time(13, 0)));

        case.now = at(14, 0);
        case.inside = true;
        assert_eq!(case.decide(afternoon).action, Action::NotNeeded);

        // A degree colder and cloudy: no sun, and not mild enough, so heat
        case.weather = Some(weather(14.0, 0.0, None, 100.0));
        let d = case.decide(afternoon);
        assert!(!d.reasons.iter().any(|r| matches!(r, Sun { .. })));
        assert_eq!(d.action, Action::Heat);

        // Clear again, the sun lifts the south room to mild, but a north room gets none at midday
        case.weather = Some(weather(14.0, 0.0, None, 0.0));
        assert_eq!(case.decide(afternoon).action, Action::NotNeeded);
        case.room.faces = vec![Facing::N];
        assert_eq!(case.decide(afternoon).action, Action::Heat);
    }

    #[test]
    fn test_no_sun_from_the_way_a_room_faces_after_dark() {
        // West faces the sun in the afternoon, but not at 18:00 in January
        let mut case = Case::new(at(17, 0), 20.5, weather(14.0, 0.0, None, 0.0));
        case.room.faces = vec![Facing::W];
        case.inside = true;
        let evening = ComfortTime {
            start: time(17, 0),
            end: time(22, 0),
            target: 20.0,
        };
        case.now = at(18, 0);
        let d = case.decide(evening);
        assert!(
            !d.reasons.iter().any(|r| matches!(r, Sun { .. })),
            "{:?}",
            d.reasons
        );
        assert_eq!(d.action, Action::Heat);

        // In summer it's still light at 18:00
        case.daylight = Some(TimePeriod::new(5, 0, 21, 30));
        assert!(case.decide(evening).reasons.contains(&Sun { gain_c: 2.0 }));

        // Without sunrise and sunset, no sun from the facing
        case.daylight = None;
        assert_eq!(case.decide(evening).action, Action::Heat);

        // A sun window counts in daylight, or when daylight isn't known, but never after dark
        let window = [TimePeriod::new(16, 0, 19, 0)];
        let with_window = |daylight| {
            let mut inputs_case = Case::new(at(18, 0), 20.5, weather(14.0, 0.0, None, 0.0));
            inputs_case.inside = true;
            inputs_case.daylight = daylight;
            decide(&Inputs {
                now: inputs_case.now,
                comfort: evening,
                inside: true,
                room_c: inputs_case.room_c,
                weather: inputs_case.weather.as_ref(),
                forecast: &[],
                room: &inputs_case.room,
                wind_exposure: WindExposure::Medium,
                sun_windows: &window,
                daylight,
                max_early_minutes: 120,
                previous: None,
            })
            .action
        };
        assert_eq!(
            with_window(Some(TimePeriod::new(8, 0, 16, 0))),
            Action::Heat
        );
        assert_eq!(
            with_window(Some(TimePeriod::new(5, 0, 21, 30))),
            Action::NotNeeded
        );
        assert_eq!(with_window(None), Action::NotNeeded);
    }

    #[test]
    fn test_the_forecast_for_the_start_is_used_over_now() {
        let mut case = Case::new(at(15, 0), 19.0, weather(10.0, 0.0, None, 100.0));
        case.forecast = vec![
            ForecastHour {
                at: at(16, 0).to_utc(),
                weather: weather(6.0, 0.0, None, 100.0),
            },
            ForecastHour {
                at: at(17, 0).to_utc(),
                weather: weather(2.0, 0.0, None, 100.0),
            },
        ];
        assert_eq!(
            case.evening().reasons[0],
            Outside {
                c: 2.0,
                forecast: true
            }
        );

        // An hour without a temperature falls back to now
        case.forecast[1].weather.temperature = None;
        assert_eq!(
            case.evening().reasons[0],
            Outside {
                c: 10.0,
                forecast: false
            }
        );

        // Converted units can carry float noise; the reasons don't
        case.weather = Some(weather(
            2.000000000000001,
            41.00000000000001,
            Some(45.2),
            100.0,
        ));
        let d = case.evening();
        assert_eq!(
            d.reasons[0],
            Outside {
                c: 2.0,
                forecast: false
            }
        );
        assert!(matches!(
            d.reasons[1],
            Wind {
                speed_kmh: 41.0,
                bearing: Some(45.0),
                ..
            }
        ));
    }

    #[test]
    fn test_missing_data_falls_back_to_the_schedule() {
        // No weather: start at the start, never later
        let mut case = Case::new(at(16, 30), 15.0, Weather::default());
        let d = case.evening();
        assert_eq!(d.reasons, vec![Indoors { c: 15.0 }, NoWeather]);
        assert_eq!((d.action, d.start_at), (Action::Off, time(17, 0)));
        case.inside = true;
        assert_eq!(case.evening().action, Action::Heat);

        // No room temperature: the schedule's On and Off
        let mut case = Case::new(at(16, 30), 15.0, weather(0.0, 30.0, None, 100.0));
        case.room_c = None;
        let d = case.evening();
        assert_eq!(
            (d.action, d.reasons.clone()),
            (Action::Off, vec![NoRoomTemperature])
        );
        case.inside = true;
        assert_eq!(case.evening().target, Some(20.0));
    }

    #[test]
    fn test_setback_between_comfort_times() {
        let mut case = Case::new(at(9, 0), 14.0, weather(5.0, 0.0, None, 100.0));
        case.room.setback_c = Some(15.0);
        let d = case.evening();
        assert_eq!((d.action, d.target), (Action::Heat, Some(15.0)));
        assert_eq!(d.reasons.last(), Some(&Setback { c: 15.0 }));
    }

    #[test]
    fn test_not_needed_holds_until_the_room_is_clearly_below_the_target() {
        let mut case = Case::new(at(18, 0), 20.0, weather(16.0, 0.0, None, 100.0));
        case.inside = true;
        let not_needed = case.evening();
        assert_eq!(not_needed.action, Action::NotNeeded);

        case.room_c = Some(19.7);
        assert_eq!(case.evening().action, Action::Heat);
        case.previous = Some(not_needed);
        assert_eq!(case.evening().action, Action::NotNeeded);
        case.room_c = Some(19.4);
        assert_eq!(case.evening().action, Action::Heat);
    }

    #[test]
    fn test_heating_that_has_begun_keeps_going_until_the_start() {
        // 16:00, 2 °C outside, room 17 °C: heating begins
        let mut case = Case::new(at(16, 0), 17.0, weather(2.0, 0.0, None, 100.0));
        let begun = case.evening();
        assert_eq!(begun.action, Action::Heat);

        // 16:40 and nearly there: a fresh decision would wait, but heating carries on from when
        // it began
        case.now = at(16, 40);
        case.room_c = Some(19.9);
        assert_eq!(case.evening().action, Action::Off);
        case.previous = Some(begun.clone());
        let d = case.evening();
        assert_eq!((d.action, d.start_at), (Action::Heat, begun.start_at));

        // Not after the comfort time: its own Heat stops at its end
        let mut evening = Case::new(at(21, 59), 20.0, weather(2.0, 0.0, None, 100.0));
        evening.inside = true;
        let during = evening.evening();
        assert_eq!(during.action, Action::Heat);
        evening.now = at(22, 0);
        evening.inside = false;
        evening.previous = Some(during);
        assert_eq!(evening.evening().action, Action::Off);

        // Not for a different comfort time, or a setback
        let mut setback = begun.clone();
        setback.reasons.push(Setback { c: 15.0 });
        case.previous = Some(setback);
        assert_eq!(case.evening().action, Action::Off);
    }

    #[test]
    fn test_comfort_time_now_or_next() {
        let mut schedule = Schedule::new("Off");
        for (start, end, on) in [(7, 9, true), (9, 17, false), (17, 22, true), (22, 7, false)] {
            let state = if on {
                HeatingState::On
            } else {
                HeatingState::Off
            };
            let entry =
                crate::schedule::ScheduleEntry::new("p", TimePeriod::new(start, 0, end, 0), state);
            schedule.add_entry(if on { entry.with_target(21.0) } else { entry });
        }
        let at = |h| comfort_time(&schedule, time(h, 0), 20.0).map(|(c, inside)| (c.start, inside));
        assert_eq!(at(8), Some((time(7, 0), true)));
        assert_eq!(at(12), Some((time(17, 0), false)));
        // Past midnight, the next is tomorrow morning's
        assert_eq!(at(23), Some((time(7, 0), false)));
        assert_eq!(comfort_time(&Schedule::new("Off"), time(8, 0), 20.0), None);
    }
}
