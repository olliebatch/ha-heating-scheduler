# What the team knows about the backend

Read this before starting a ticket, and add to it before one ships.

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
  shared with the API, which forgets an entity's entries when it's added or removed. Keep the
  two in step.
- TRV readings used to build a tick's `Scheduled` are the *previous* tick's (the cached state
  from before this tick's fetch). A failed fetch leaves the old cached state in place, so check
  `EntityMemory.read` before trusting a reading.

## Gotchas
- The frost floor knows it started the heating from `last_sent == Some(FROST_TARGET_C)` (10 °C).
  A schedule target of exactly 10 °C looks the same. Don't send 10 °C for any other reason
  without changing that check.
- Driving TRVs harder (gh-32): the TRV offset applies only to the drive reading. Smart and
  Shadow decisions use `shadow::room_temperature`, which has no offset. A zone with a room sensor
  is driven only by that sensor and never falls back to its TRVs.
- `Room` is `#[serde(default)]`: a new room field needs a default so older zones.json files load.
  Request bodies use `strict_room`, which picks up new fields automatically from `Room::default()`.
- Anything that changes what's sent to HA, how DRY_RUN is read, or the `data/*.json` format is
  held for Ollie, and every backend PR states its panel impact.
