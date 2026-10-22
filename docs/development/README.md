# Backend development notes

How the backend works, its conventions and its pitfalls. Read it before changing the code, and
add to it in the same change when you learn something the next contributor would want.

## Where things live
- `src/scheduler/mod.rs`: the loop (`run_scheduler`), what each entity is sent
  (`apply_schedule_to_entity`), zone status, frost floor, put-back and drive constants.
- `src/shadow.rs` / `src/smart.rs`: Shadow logging and the Smart decision; `shadow::room_temperature`
  is the room reading Smart uses.
- `src/zones/` (`room.rs` for a zone's mode and room settings) and `src/server/zones.rs` (the zone
  API: `GET /zones` builds each status, `PATCH /zones/{id}` takes the room).
- `src/server/handlers.rs`: entities add and remove; `src/main.rs` wires the shared state.
- `tests/dry_run.rs`: the fake HA. `README.md` documents every API field; update it with the code.

## Testing and checking
- `cargo fmt --check && cargo test` is the check; `cargo build --all-targets` builds the tests too.
  `cargo clippy --all-targets` has 3 old warnings (module inception, a derivable `Default`, a
  `default()` method); don't add to them.
- `scripts/demo.sh` can't show `GET /dry_run`: debug builds use `MockClimate`, which never goes
  through the HA client, so nothing is recorded, and `MockWeather` has no room sensors. To show
  what a dry run sends, use a fake HA, as `tests/dry_run.rs` does (`DefaultClimate` plus a
  dry-run `ApiClient`).
- `apply_schedule_to_entity` is the unit to test what's sent: tick a `MockClimate` with
  `Scheduled` values and check `set_temperatures`. Carry `last_sent` and `EntityMemory` across
  ticks when the behaviour depends on the previous tick (put-back, driving).
- An unreachable `ApiClient` (`http://127.0.0.1:1`) with a `DefaultClimate` makes a failed fetch.

## How the scheduler loop works (src/scheduler/mod.rs)
- Every 15 s it copies the entities, reads the weather, forecast and room sensors, works out each
  entity's `Scheduled` (zone status, or the smart decision for smart zones), then applies each one
  outside the locks.
- What's kept per entity between ticks: `LastSentTargets` (the last target sent) and
  `EntityMemories` (put-back time, driving, the room reading, whether it was read). Both are
  shared with the API. `add_entities` and `remove_entity` (src/server/handlers.rs) must forget an
  entity in **both** maps, or a re-added TRV inherits stale state; a new map would need the same.
- TRV readings used to build a tick's `Scheduled` are the *previous* tick's (the cached state
  from before this tick's fetch). A failed fetch leaves the old cached state in place, so check
  `EntityMemory.read` before trusting a reading.

## Gotchas
- The frost floor knows it started the heating from `last_sent == Some(FROST_TARGET_C)` (10 °C).
  A schedule target of exactly 10 °C looks the same. Don't send 10 °C for any other reason
  without changing that check.
- Driving TRVs harder: the TRV offset applies only to the drive reading. Smart and
  Shadow decisions use `shadow::room_temperature`, which has no offset. A zone with a room sensor
  is driven only by that sensor and never falls back to its TRVs.
- Room sensor readings have no age check: an unavailable or unreadable sensor gives None, but a
  sensor that stops updating while still reporting a number is trusted.
- `apply_schedule_to_entity` has 7 parameters, clippy's limit. Put new per-entity state in
  `EntityMemory` rather than adding a parameter.
- `Room` is `#[serde(default)]`: a new room field needs a default so older zones.json files load.
  Request bodies use `strict_room`, which picks up new fields automatically from `Room::default()`.
  A new room field is also safe from the panel only because the panel (ha-heating-scheduler-panel,
  `ZoneWeather.tsx`, `ZoneRoom.tsx`) PATCHes back the whole `zone.room` it got from `GET /zones`.
  If it ever builds the room from its own TS type, new fields get reset to their defaults, so
  check the panel before adding one.
- Reviewing what's sent to HA: for every new target source, trace `last_sent`, put-back and the
  frost floor. Repeated-call bugs come from a target that changes every tick (weather adjust)
  and isn't compared against what was actually sent.
- Take extra care with, and call out in the PR, any change to what's sent to Home Assistant
  (turning heating on or off, targets, boosts), how DRY_RUN is parsed or honoured, or the
  `data/*.json` format: these control a real house's heating or its saved settings. Say too
  whether the panel needs a matching change.
