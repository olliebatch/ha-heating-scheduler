//! Dry run: read Home Assistant but never change it. The HA client records each call it would
//! have made here instead of sending it, and answers the caller as if HA had accepted it, so the
//! scheduler behaves exactly as it would for real.

use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Mutex;

/// How many would-be calls to keep; older ones are dropped
const KEEP_CALLS: usize = 200;

tokio::task_local! {
    /// Why the current call is being made, e.g. "Lounge: 20 °C scheduled, +1.5 cold";
    /// set by the scheduler around each entity it applies
    pub static CALL_CONTEXT: String;
}

/// A call the backend would have made to Home Assistant
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WouldBeCall {
    /// When it was first wanted
    pub at: DateTime<Local>,
    /// When it was last wanted: HA's real state doesn't change in a dry run, so the scheduler
    /// keeps asking for the same thing; repeats are counted rather than listed
    pub last_at: DateTime<Local>,
    pub repeats: u32,
    pub method: String,
    pub endpoint: String,
    pub body: Value,
    /// e.g. "would set climate.lounge_trv to 21.5 °C"
    pub summary: String,
    pub context: Option<String>,
}

/// The would-be calls of a dry run, newest first
#[derive(Debug, Default)]
pub struct DryRun {
    calls: Mutex<VecDeque<WouldBeCall>>,
}

/// Whether to dry-run: `--dry-run`, or `DRY_RUN` set to 1/true/yes/on (on) or 0/false/no/off
/// (off), in any case. Unset is off. Anything else is an error, so the backend refuses to start:
/// a `DRY_RUN` value it doesn't know (empty included), or any command-line argument other than
/// `--dry-run` (a mistyped `--dryrun` must never quietly control the real heating). `args`
/// includes the program name first, as `std::env::args` gives it.
pub fn enabled(
    args: impl IntoIterator<Item = String>,
    env: Option<String>,
) -> Result<bool, String> {
    let mut dry_run_flag = false;
    for arg in args.into_iter().skip(1) {
        if arg == "--dry-run" {
            dry_run_flag = true;
        } else {
            return Err(format!(
                "unknown argument {arg:?}; the only option is --dry-run; \
                 not starting, so this can't control the heating by mistake"
            ));
        }
    }
    if dry_run_flag {
        return Ok(true);
    }
    let Some(value) = env else {
        return Ok(false);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "DRY_RUN must be 1, true, yes or on (dry run) or 0, false, no or off, not {value:?}; \
             not starting, so this can't control the heating by mistake"
        )),
    }
}

/// Requests that only read Home Assistant, so they're still sent in a dry run. Anything not
/// listed is treated as a change. `/api/template` renders a template (a read) but is a POST,
/// which is why this lists reads rather than blocking POSTs.
pub fn is_read(method: &reqwest::Method, endpoint: &str) -> bool {
    (method == reqwest::Method::GET && endpoint.starts_with("/api/states/"))
        || (method == reqwest::Method::POST && endpoint == "/api/template")
}

impl DryRun {
    /// Record a call instead of making it. An identical call for the same entity and endpoint
    /// as that entity's latest one only counts as a repeat, so the log has one entry per change.
    pub fn record(
        &self,
        method: &reqwest::Method,
        endpoint: &str,
        body: &Value,
        now: DateTime<Local>,
    ) {
        let context = CALL_CONTEXT.try_with(Clone::clone).ok();
        let summary = summarise(endpoint, body);
        println!(
            "[DRY RUN] {}{}",
            summary,
            context
                .as_ref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default()
        );

        let mut calls = self.calls.lock().unwrap();
        let entity = body.get("entity_id");
        let latest = calls
            .iter_mut()
            .find(|c| c.endpoint == endpoint && c.body.get("entity_id") == entity);
        if let Some(latest) = latest.filter(|c| c.method == method.as_str() && &c.body == body) {
            latest.repeats += 1;
            latest.last_at = now;
            return;
        }
        calls.push_front(WouldBeCall {
            at: now,
            last_at: now,
            repeats: 0,
            method: method.to_string(),
            endpoint: endpoint.to_string(),
            body: body.clone(),
            summary,
            context,
        });
        calls.truncate(KEEP_CALLS);
    }

    /// The would-be calls, newest first
    pub fn calls(&self) -> Vec<WouldBeCall> {
        self.calls.lock().unwrap().iter().cloned().collect()
    }
}

/// A plain description of a call, e.g. "would set climate.lounge_trv to 21.5 °C"
fn summarise(endpoint: &str, body: &Value) -> String {
    let entity = body.get("entity_id").and_then(Value::as_str).unwrap_or("?");
    match endpoint {
        "/api/services/climate/set_temperature" => {
            match body.get("temperature").and_then(Value::as_f64) {
                Some(t) => format!("would set {entity} to {t} °C"),
                None => format!("would set {entity}'s temperature"),
            }
        }
        "/api/services/climate/set_hvac_mode" => {
            match body.get("hvac_mode").and_then(Value::as_str) {
                Some("off") => format!("would turn {entity} off"),
                Some(mode) => format!("would turn {entity} on ({mode})"),
                None => format!("would set {entity}'s mode"),
            }
        }
        _ => format!("would call {endpoint} with {body}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Method;
    use serde_json::json;

    fn set(entity: &str, t: f64) -> Value {
        json!({"entity_id": entity, "temperature": t})
    }
    const SET: &str = "/api/services/climate/set_temperature";
    const MODE: &str = "/api/services/climate/set_hvac_mode";

    #[test]
    fn test_enabled_fails_closed() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let env = |v: &str| Some(v.to_string());
        assert_eq!(enabled(args(&["bin"]), None), Ok(false));
        assert_eq!(enabled(args(&["bin", "--dry-run"]), None), Ok(true));
        for on in [
            "1", "true", "TRUE", "True", "yes", "Yes", "on", "ON", " on ",
        ] {
            assert_eq!(enabled(args(&["bin"]), env(on)), Ok(true), "{on:?}");
        }
        for off in ["0", "false", "False", "no", "NO", "off", "Off"] {
            assert_eq!(enabled(args(&["bin"]), env(off)), Ok(false), "{off:?}");
        }
        // Anything else refuses to start rather than running live
        for unknown in ["", " ", "y", "2", "enabled", "dry", "tru"] {
            let err = enabled(args(&["bin"]), env(unknown)).unwrap_err();
            assert!(err.contains("not starting"), "{unknown:?}: {err}");
        }
        // --dry-run wins over any DRY_RUN, so it can only make things safer
        assert_eq!(enabled(args(&["bin", "--dry-run"]), env("0")), Ok(true));
        assert_eq!(enabled(args(&["bin", "--dry-run"]), env("bogus")), Ok(true));
        assert_eq!(
            enabled(args(&["bin", "--dry-run", "--dry-run"]), None),
            Ok(true)
        );
    }

    #[test]
    fn test_unknown_arguments_refuse_to_start() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for bad in [
            "--dryrun",
            "--dry_run",
            "-dry-run",
            "--Dry-Run",
            "dry-run",
            "-n",
            "--help",
            "",
        ] {
            let err = enabled(args(&["bin", bad]), None).unwrap_err();
            assert!(
                err.contains("not starting") && err.contains(&format!("{bad:?}")),
                "{bad:?}: {err}"
            );
        }
        // Even next to a correct --dry-run, and whatever DRY_RUN says
        assert!(enabled(args(&["bin", "--dry-run", "--dryrun"]), None).is_err());
        assert!(enabled(args(&["bin", "--dryrun"]), Some("1".to_string())).is_err());
        // The program name itself is never an argument
        assert_eq!(enabled(args(&["--dryrun"]), None), Ok(false));
    }

    #[test]
    fn test_reads_are_allowed_and_everything_else_is_a_change() {
        assert!(is_read(&Method::GET, "/api/states/climate.lounge_trv"));
        assert!(is_read(&Method::POST, "/api/template"));
        assert!(!is_read(&Method::POST, SET));
        assert!(!is_read(&Method::POST, "/api/states/climate.lounge_trv"));
        assert!(!is_read(
            &Method::GET,
            "/api/services/climate/set_temperature"
        ));
        assert!(!is_read(&Method::DELETE, "/api/states/climate.lounge_trv"));
        assert!(!is_read(&Method::POST, "/api/template/extra"));
    }

    #[test]
    fn test_one_entry_per_change() {
        let dry = DryRun::default();
        let now = Local::now();
        for _ in 0..5 {
            dry.record(
                &Method::POST,
                MODE,
                &json!({"entity_id": "climate.a", "hvac_mode": "heat"}),
                now,
            );
            dry.record(&Method::POST, SET, &set("climate.a", 21.5), now);
            dry.record(&Method::POST, SET, &set("climate.b", 20.0), now);
        }
        dry.record(&Method::POST, SET, &set("climate.a", 22.0), now);

        let summaries: Vec<(String, u32)> = dry
            .calls()
            .into_iter()
            .map(|c| (c.summary, c.repeats))
            .collect();
        assert_eq!(
            summaries,
            vec![
                ("would set climate.a to 22 °C".to_string(), 0),
                ("would set climate.b to 20 °C".to_string(), 4),
                ("would set climate.a to 21.5 °C".to_string(), 4),
                ("would turn climate.a on (heat)".to_string(), 4),
            ]
        );
        // Back to 21.5 after 22 is a new change, not a repeat of the older entry
        dry.record(&Method::POST, SET, &set("climate.a", 21.5), now);
        assert_eq!(dry.calls()[0].summary, "would set climate.a to 21.5 °C");
        assert_eq!(dry.calls()[0].repeats, 0);
    }

    #[tokio::test]
    async fn test_context_and_bounded_log() {
        let dry = DryRun::default();
        let now = Local::now();
        CALL_CONTEXT
            .scope("Lounge: 20 °C scheduled".to_string(), async {
                dry.record(
                    &Method::POST,
                    MODE,
                    &json!({"entity_id": "climate.a", "hvac_mode": "off"}),
                    now,
                );
            })
            .await;
        let call = &dry.calls()[0];
        assert_eq!(call.summary, "would turn climate.a off");
        assert_eq!(call.context.as_deref(), Some("Lounge: 20 °C scheduled"));

        for i in 0..250 {
            dry.record(&Method::POST, SET, &set(&format!("climate.{i}"), 20.0), now);
        }
        let calls = dry.calls();
        assert_eq!(calls.len(), KEEP_CALLS);
        assert_eq!(calls[0].body["entity_id"], "climate.249");
    }
}
