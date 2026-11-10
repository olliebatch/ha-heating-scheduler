# Backend development notes

How the backend works, its conventions and its pitfalls. Read it before changing the code, and
add to it in the same change when you learn something the next contributor would want.

## Where things live
- `src/scheduler/mod.rs`: the loop (`run_scheduler`), what each entity is sent
  (`apply_schedule_to_entity`), zone status, frost floor, put-back and drive constants.
- `src/shadow.rs` / `src/smart.rs`: Shadow logging and the Smart decision. The room reading Smart
  and Shadow use is `scheduler::smart_reading`: `shadow::room_temperature` over only the TRVs read
  on their last tick.
- `src/zones/` (`room.rs` for a zone's mode and room settings) and `src/server/zones.rs` (the zone
  API: `GET /zones` builds each status, `PATCH /zones/{id}` takes the room).
- `src/server/handlers.rs`: entities add and remove; `src/main.rs` wires the shared state.
- `tests/dry_run.rs`: the fake HA. `README.md` documents every API field and every status each route can return (including
  axum's 422 when a JSON body doesn't parse); update it with the code.

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
- `tests/router.rs` drives the real router (`server::router`) with `oneshot`, over mock state in a
  temp dir. Add every new route to its `ROUTES` table (method and path); a route missing from the
  table isn't covered. It checks each route isn't a router 404/405 (a handler's own "not found"
  has a body, so it passes), that other methods on the path give 405, and that unknown paths give 404.
- An unreachable `ApiClient` (`http://127.0.0.1:1`) with a `DefaultClimate` makes a failed fetch.
- Handler tests call the handler functions directly (`test_state()` in `src/server/handlers.rs`),
  so routes and request-body parsing in `start_server` are untested. Known gap: extract the router
  into `fn router(state)` so routes can be tested with tower's `oneshot`.

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
  `EntityMemory.read` before trusting a reading: `drive_reading` and `smart_reading` do. Don't
  call `shadow::room_temperature` on all the entities for a decision.
- A smart zone's TRVs on the first tick after a start (`smart_waiting`) are fetched in
  `run_scheduler`'s `let Some(scheduled) = scheduled else` branch, not by
  `apply_schedule_to_entity`. Anything apply records per entity (like `EntityMemory.read`) must
  be recorded there too.
- `run_scheduler` has no test harness. Keep logic out of it, in functions you can test
  (`smart_reading`, `smart_waiting`), and check anything left inline against a fake HA.
- Decisions are made before the tick's fetch, so the tick on which a TRV comes back still decides
  without it (and that decision is sent); driving also pauses for that tick. Its reading counts
  from the next tick.

## Schedule sets and days (src/schedule/sets.rs)
- Which schedule an entity follows: its zone's `schedule_set_id` if set, else `ScheduleSets::for_day`
  (the set assigned to today's weekday in `day_sets`, else the active set). Only two places
  resolve it: `scheduler::zone_status` (the loop, `GET /zones`, the zone preview) and
  `shadow::zone_tick` (Smart and Shadow). Any new heating decision must resolve it the same way,
  with `for_day`, not `sets.active()`, and needs its own test: swapping either existing line back
  to `active()` fails only that line's test (`test_scheduled_follows_the_days_set`,
  `test_zone_tick_follows_the_days_set`).
- `active()` alone is right only for the legacy `/schedule` routes, which edit the active set.
- `day_sets` maps each weekday to at most one set, so overlap can't be stored; `assign_days`
  checks every day before changing anything and refuses a day held by another set. Deleting a
  set with days is refused, like one a zone follows.
- Days are `chrono::Weekday`, saved as "Mon".."Sun"; requests also take full names and any case.

### Midnight
- The set is picked by the date of `now`; at 00:00 the next day's set takes over, so a period
  crossing midnight switches sets part way through.
- Early starts (weather and Smart) look ahead only within today's set: an early start that
  would begin before midnight for a period in tomorrow's (different) set doesn't happen.
- A weather late finish (`continue_late_finish`) begun for a period ending at 00:00 stops at
  midnight if the next day's set has no On period ending at 00:00. Consistent with the rule
  above, but a known edge if heating is reported stopping at midnight.

## Gotchas
- A new optional field in a `data/*.json` file needs `#[serde(default)]` and
  `skip_serializing_if` for its empty value. `load_or_migrate` saves whenever the loaded JSON
  differs from the file, so without the skip every existing install's file is rewritten on its
  next start (`day_sets` does this). Check by restarting on the same data: the file's sha and
  mtime should not change.
- Tests that depend on the date pin it with `Local.with_ymd_and_hms` (2026-11-14 is a Saturday,
  2026-11-16 a Monday) and assert the weekday, rather than using `Local::now()`.
- `smart::decide` has early returns (no room temperature, no weather) for falling back to the
  schedule. A fallback that ignores the "begun" check cancels a warm-up under way: both of
  these keep a begun Heat (and its `Lead` reason, which is what marks it begun). Keep a begun
  warm-up in any new fallback, and copy the whole three-arm `match` (inside the comfort time,
  then begun, then `between`): the order matters, because inside the comfort time it's the
  schedule's Heat, not the begun one, that must win.
- `ComfortTime` has no date, so tomorrow's same period compares equal. A warm-up stops counting
  as begun only because every tick inside the comfort time drops the `Lead` reason; keep that
  true if you change the decision inside the comfort time.
- `Shadow::load` restores the decisions log but not the latest `decisions`, so after a restart
  there's no `previous`: no hysteresis and no begun warm-up carry over.
- The top-level readme is `README.MD` (upper-case extension) in git. It opens as `README.md` on
  a case-insensitive filesystem but not on Linux, so use the real name in scripts.
- The frost floor knows it started the heating from `last_sent == Some(FROST_TARGET_C)` (10 °C).
  A schedule target of exactly 10 °C looks the same. Don't send 10 °C for any other reason
  without changing that check.
- A boost sends `max_temp` (`boost_max` in `apply_schedule_to_entity`), ahead of the frost floor
  and driving. A boosted entity isn't driving (`memory.driving` false), so when the boost ends
  driving is decided afresh from the room, like a TRV that was never driven. One with no
  `max_temp` known gets the scheduled or default target, as before. The only reader of
  `memory.driving`/`drive_setpoint` outside the scheduler is `zone_drive` (`GET /zones`), which
  reports a boosted TRV as not driven.
- Cancelling a boost (`POST /boost/cancel`, `POST /boost_all/cancel`) is just `set_boost(None)`. The
  next tick sees an unboosted entity whose `last_sent` is the boost's `max_temp`, so the usual target
  differs and is sent. An empty `climate_names` cancels nothing, unlike `/boost_all/cancel`.
- Driving TRVs harder: the TRV offset applies only to the drive reading. Smart and
  Shadow decisions use `smart_reading`, which has no offset. A zone with a room sensor
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
