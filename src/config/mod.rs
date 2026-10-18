pub mod entities_persistence;

use std::path::Path;

pub struct Config {
    pub ha_url: String,
    pub ha_token: String,
    pub climate_entities: Vec<String>,
    pub data_path: String,
}

/// The port to listen on: `PORT`, or 3000
pub fn port(env: Option<String>) -> anyhow::Result<u16> {
    match env.as_deref().map(str::trim) {
        None | Some("") => Ok(3000),
        Some(port) => port
            .parse()
            .map_err(|_| anyhow::anyhow!("PORT must be a port number, not {port:?}")),
    }
}

/// The startup line naming the timezone that schedule times are read in, and a warning when it may
/// not be the one meant. `tz` is `TZ`; `localtime` is where `/etc/localtime` links to (itself when
/// it's a plain file), if it's there; `zone_exists` says whether a zone file exists for a name.
/// Without either, the clock is UTC.
pub fn timezone(
    tz: Option<&str>,
    localtime: Option<&Path>,
    zone_exists: impl Fn(&str) -> bool,
    offset: chrono::FixedOffset,
) -> (String, Option<String>) {
    match tz.map(|tz| tz.trim().trim_start_matches(':')) {
        Some(tz) if !tz.is_empty() => {
            let warning = (tz != "UTC" && !zone_exists(tz)).then(|| {
                format!(
                    "=== WARNING: TZ={tz}, BUT THERE'S NO SUCH TIMEZONE ===\n\
                     Is tzdata installed? Without it, schedule times are UTC."
                )
            });
            (format!("Timezone: {tz} (from TZ), now {offset}"), warning)
        }
        _ => match localtime {
            Some(path) => {
                let path = path.to_string_lossy();
                let line = match path.split_once("zoneinfo/") {
                    Some((_, name)) => {
                        format!("Timezone: {name} (from /etc/localtime), now {offset}")
                    }
                    None => format!("Timezone: from {path}, now {offset}"),
                };
                (line, None)
            }
            None => (
                format!("Timezone: UTC, as none is set, now {offset}"),
                Some(
                    "=== WARNING: NO TIMEZONE SET, SO SCHEDULE TIMES ARE UTC ===\n\
                     Set TZ to yours, e.g. TZ=Europe/London."
                        .to_string(),
                ),
            ),
        },
    }
}

/// [`timezone`] for this machine
pub fn system_timezone() -> (String, Option<String>) {
    let localtime = Path::new("/etc/localtime");
    let localtime = std::fs::read_link(localtime)
        .ok()
        .or_else(|| localtime.exists().then(|| localtime.to_path_buf()));
    let zoneinfo = std::env::var("TZDIR").unwrap_or_else(|_| "/usr/share/zoneinfo".to_string());
    timezone(
        std::env::var("TZ").ok().as_deref(),
        localtime.as_deref(),
        |zone| Path::new(&zoneinfo).join(zone).is_file(),
        *chrono::Local::now().offset(),
    )
}

impl Config {
    pub fn new(
        ha_url: &str,
        ha_token: &str,
        climate_entities: Vec<String>,
        data_path: String,
    ) -> Self {
        Config {
            ha_url: ha_url.to_string(),
            ha_token: ha_token.to_string(),
            climate_entities,
            data_path,
        }
    }

    /// Load config from environment variables only (legacy method)
    /// Climate entities are loaded from CLIMATE_ENTITY env var
    pub fn from_env() -> Self {
        dotenv::dotenv().ok();
        let ha_url = std::env::var("HA_URL").expect("HA_URL must be set");
        let ha_token = std::env::var("HA_TOKEN").expect("HA_TOKEN must be set");
        let climate_entity = std::env::var("CLIMATE_ENTITY").expect("CLIMATE_ENTITY must be set");
        let data_path = std::env::var("DATA_PATH").expect("DATA_PATH must be set");
        let climates: Vec<String> = climate_entity
            .split(",")
            .map(|s| s.trim().to_owned())
            .collect();
        Config::new(&ha_url, &ha_token, climates, data_path)
    }

    /// Load config with entities from persisted file
    /// Falls back to environment variable if file doesn't exist or is empty
    /// If loaded from env var, saves to file for future use (migration path)
    pub fn from_env_with_persisted_entities() -> anyhow::Result<Self> {
        dotenv::dotenv().ok();
        let ha_url = std::env::var("HA_URL").expect("HA_URL must be set");
        let ha_token = std::env::var("HA_TOKEN").expect("HA_TOKEN must be set");
        let data_path = std::env::var("DATA_PATH").expect("DATA_PATH must be set");

        // Try to load entities from persisted file
        let entities_file_path = Path::new(&data_path).join("entities.json");
        let entities_config = entities_persistence::load_or_create_default(&entities_file_path)?;

        let climate_entities = if entities_config.climate_entities.is_empty() {
            // Fall back to environment variable if no entities in file
            println!("No entities in persisted file, checking environment variable...");
            if let Ok(climate_entity) = std::env::var("CLIMATE_ENTITY") {
                let climates: Vec<String> = climate_entity
                    .split(",")
                    .map(|s| s.trim().to_owned())
                    .collect();
                println!(
                    "Loaded {} entities from CLIMATE_ENTITY env var",
                    climates.len()
                );

                // Save to file so next time we don't need the env var
                let new_config = entities_persistence::EntitiesConfig::new(climates.clone());
                if let Err(e) =
                    entities_persistence::save_entities(&new_config, &entities_file_path)
                {
                    eprintln!(
                        "Warning: Failed to save entities from env var to file: {}",
                        e
                    );
                } else {
                    println!(
                        "Saved entities to {} for future use",
                        entities_file_path.display()
                    );
                    println!("You can now remove CLIMATE_ENTITY from your .env file");
                }

                climates
            } else {
                println!("No CLIMATE_ENTITY env var found. Starting with empty entities list.");
                Vec::new()
            }
        } else {
            println!(
                "Loaded {} entities from persisted file",
                entities_config.climate_entities.len()
            );
            entities_config.climate_entities
        };

        Ok(Config::new(&ha_url, &ha_token, climate_entities, data_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port() {
        assert_eq!(port(None).unwrap(), 3000);
        assert_eq!(port(Some(String::new())).unwrap(), 3000);
        assert_eq!(port(Some("3100".to_string())).unwrap(), 3100);
        assert!(port(Some("http".to_string())).is_err());
        assert!(port(Some("70000".to_string())).is_err());
    }

    #[test]
    fn test_timezone() {
        let bst = chrono::FixedOffset::east_opt(3600).unwrap();
        let utc = chrono::FixedOffset::east_opt(0).unwrap();
        let london = |zone: &str| zone == "Europe/London";
        let link = Path::new("/usr/share/zoneinfo/Europe/London");

        let (line, warning) = timezone(Some("Europe/London"), None, london, bst);
        assert_eq!(line, "Timezone: Europe/London (from TZ), now +01:00");
        assert_eq!(warning, None);

        // TZ wins over /etc/localtime, and may start with ':'
        let (line, _) = timezone(Some(":Europe/London"), Some(Path::new("/x")), london, bst);
        assert_eq!(line, "Timezone: Europe/London (from TZ), now +01:00");

        let (line, warning) = timezone(None, Some(link), london, bst);
        assert_eq!(
            line,
            "Timezone: Europe/London (from /etc/localtime), now +01:00"
        );
        assert_eq!(warning, None);
        // A plain file, e.g. mounted into a container, has no name
        let (line, _) = timezone(None, Some(Path::new("/etc/localtime")), london, bst);
        assert_eq!(line, "Timezone: from /etc/localtime, now +01:00");

        // Neither: UTC, loudly
        let (line, warning) = timezone(Some(""), None, london, utc);
        assert_eq!(line, "Timezone: UTC, as none is set, now +00:00");
        assert!(warning.unwrap().contains("NO TIMEZONE SET"));

        // A zone with no zone file (no tzdata) is UTC too
        let (_, warning) = timezone(Some("Europe/Paris"), None, london, utc);
        assert!(warning.unwrap().contains("TZ=Europe/Paris, BUT"));
        // UTC asked for is no surprise
        assert_eq!(timezone(Some("UTC"), None, london, utc).1, None);
    }
}
