//! How a zone is heated (by its schedule, or by the smart decisions) and what the smart decisions
//! know about the room. Nothing here changes how a zone is heated yet.

use serde::{Deserialize, Serialize};

/// How a zone is heated
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneMode {
    /// On and Off as the schedule says, with the weather rules the zone has switched on
    #[default]
    Schedule,
    /// As Schedule, while working out (and logging) what Smart would have done
    Shadow,
    /// The schedule's On periods are comfort times, and the service decides when to heat
    Smart,
}

/// A side of the house a room faces
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Facing {
    N,
    E,
    S,
    W,
}

/// What the smart decisions know about a room. The defaults behave close to today. Wind
/// exposure is the zone's existing `wind_exposure`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Room {
    /// The sides the room faces; none means wind and sun direction are ignored
    pub faces: Vec<Facing>,
    /// How much the sun warms the room, 0 (none) to 1 (lots)
    pub sun_gain: f64,
    /// How fast the room loses heat, 0 (well insulated) to 1 (draughty)
    pub heat_loss: f64,
    /// How fast the room warms while heating, in °C per hour
    pub warm_rate_c_per_hour: f64,
    /// Between comfort times, heat only if the room drops below this (°C); None leaves it off
    pub setback_c: Option<f64>,
    /// A temperature sensor to read the room from; None uses the average of the zone's TRVs
    pub sensor_entity_id: Option<String>,
}

impl Default for Room {
    fn default() -> Self {
        Room {
            faces: Vec::new(),
            sun_gain: 0.5,
            heat_loss: 0.5,
            warm_rate_c_per_hour: 1.5,
            setback_c: None,
            sensor_entity_id: None,
        }
    }
}

/// Allowed `warm_rate_c_per_hour`
pub const WARM_RATE_RANGE: std::ops::RangeInclusive<f64> = 0.5..=4.0;
/// Allowed `setback_c`
pub const SETBACK_RANGE: std::ops::RangeInclusive<f64> = 10.0..=18.0;

/// A room in a request: like [`Room`], but a field it doesn't know is an error, so a misspelt
/// one can't reset the room to its defaults. (A saved room stays lenient, so a file from a later
/// version still loads.) Null or left out is None.
pub fn strict_room<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Room>, D::Error> {
    use serde::de::Error;
    let Some(value) = Option::<serde_json::Value>::deserialize(d)? else {
        return Ok(None);
    };
    let known = serde_json::to_value(Room::default()).map_err(D::Error::custom)?;
    let known: Vec<&String> = known
        .as_object()
        .into_iter()
        .flat_map(|o| o.keys())
        .collect();
    if let Some(unknown) = value
        .as_object()
        .and_then(|o| o.keys().find(|k| !known.contains(k)))
    {
        return Err(D::Error::custom(format!(
            "unknown field `{unknown}`, expected one of {}",
            known
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(D::Error::custom)
}

impl Room {
    /// The room with its sensor id trimmed (blank meaning none), or why it isn't valid
    pub fn validated(mut self) -> Result<Room, String> {
        let in_range = |name: &str, value: f64, range: std::ops::RangeInclusive<f64>| {
            if range.contains(&value) {
                Ok(())
            } else {
                Err(format!(
                    "{name} must be {}-{}, not {value}",
                    range.start(),
                    range.end()
                ))
            }
        };
        in_range("sun_gain", self.sun_gain, 0.0..=1.0)?;
        in_range("heat_loss", self.heat_loss, 0.0..=1.0)?;
        in_range(
            "warm_rate_c_per_hour",
            self.warm_rate_c_per_hour,
            WARM_RATE_RANGE,
        )?;
        if let Some(setback) = self.setback_c {
            in_range("setback_c", setback, SETBACK_RANGE)?;
        }
        let mut seen = Vec::new();
        self.faces.retain(|f| {
            let new = !seen.contains(f);
            seen.push(*f);
            new
        });
        self.sensor_entity_id = self
            .sensor_entity_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(sensor) = &self.sensor_entity_id {
            if !sensor.starts_with("sensor.") || sensor.len() == "sensor.".len() {
                return Err(format!(
                    "sensor_entity_id must be a sensor.* entity id, not {sensor}"
                ));
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_fields_left_out_get_defaults() {
        let room: Room = serde_json::from_value(json!({"faces": ["N", "E"]})).unwrap();
        assert_eq!(room.faces, vec![Facing::N, Facing::E]);
        assert_eq!(room.warm_rate_c_per_hour, 1.5);
        assert_eq!(room.setback_c, None);
        assert_eq!(
            serde_json::from_value::<ZoneMode>(json!("shadow")).unwrap(),
            ZoneMode::Shadow
        );
    }

    #[test]
    fn test_validated() {
        let room = |v: serde_json::Value| serde_json::from_value::<Room>(v).unwrap().validated();

        let ok =
            room(json!({"faces": ["S", "S", "W"], "sensor_entity_id": " sensor.lounge "})).unwrap();
        assert_eq!(ok.faces, vec![Facing::S, Facing::W]);
        assert_eq!(ok.sensor_entity_id.as_deref(), Some("sensor.lounge"));
        assert_eq!(
            room(json!({"sensor_entity_id": ""}))
                .unwrap()
                .sensor_entity_id,
            None
        );
        assert!(room(json!({"setback_c": 15})).is_ok());

        for (bad, field) in [
            (json!({"sun_gain": 1.5}), "sun_gain"),
            (json!({"heat_loss": -0.1}), "heat_loss"),
            (json!({"warm_rate_c_per_hour": 0.2}), "warm_rate_c_per_hour"),
            (json!({"setback_c": 20}), "setback_c"),
            (
                json!({"sensor_entity_id": "climate.lounge_trv"}),
                "sensor_entity_id",
            ),
            (json!({"sensor_entity_id": "sensor."}), "sensor_entity_id"),
        ] {
            let error = room(bad).unwrap_err();
            assert!(error.starts_with(field), "{error}");
        }
        assert!(serde_json::from_value::<Room>(json!({"faces": ["NE"]})).is_err());
        // A saved room with a field this version doesn't know still loads
        let later: Room = serde_json::from_value(json!({"faces": ["N"], "comfort_c": 20})).unwrap();
        assert_eq!(later.faces, vec![Facing::N]);
    }
}
