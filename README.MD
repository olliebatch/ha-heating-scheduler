# Home Assistant Heating Scheduler

A work in progress to build a more complex home assistant heating schedule that allows easy control over the schedules and extra functionalities easily accessible.

## Configuration

Create a `.env` file:

```env
HA_URL=http://your-home-assistant:8123
HA_TOKEN=your-long-lived-access-token
DATA_PATH=./data
```

Optional: `PORT` (default 3000) for the API, and `DRY_RUN=1` to read Home Assistant without changing it (see
"Dry run against your Home Assistant"). `DRY_RUN` takes 1/true/yes/on or 0/false/no/off, in any case; any other
value stops the backend at startup, so a typo can't run it live.

### Entity Management

Climate entities are stored in `data/entities.json` and can be managed via API.

**Migration from Environment Variables:**
- On first run, entities from `CLIMATE_ENTITY` env var are automatically saved to `entities.json`
- After first run, you can remove `CLIMATE_ENTITY` from your `.env` file
- Example env var format:
```env
CLIMATE_ENTITY=climate.living_room,climate.bedroom
```

## API Endpoints

### Schedule sets
Schedules are kept as named sets in `data/schedule_sets.json`; one is active and drives the heating.
On first run, an existing `data/schedule.json` becomes the only (active) set and is left on disk as a backup.

- `GET /schedule_sets` - All sets and the `active_id`
- `POST /schedule_sets` - Create a set: `{"name": "Holiday", "copy_from": "<set id>"}` (`copy_from` optional; without it the set is Off all day). Returns the set
- `PATCH /schedule_sets/{id}` - Rename: `{"name": "Away"}`. Returns the set
- `DELETE /schedule_sets/{id}` - Delete a set (409 for the active or last set). Returns all sets
- `POST /schedule_sets/{id}/activate` - Make a set active. Returns all sets
- `POST /schedule_sets/{id}/entries` - Add an entry to a set (400 if start equals end, except 00:00 - 00:00). Returns the set.
  On entries need a `target_temp` between 5 and 30 °C; Off entries have none, e.g.
  `{"name": "Morning", "time_period": {"start": "06:00", "end": "09:00"}, "heating_state": "ON", "target_temp": 21}`
- `DELETE /schedule_sets/{id}/entries/{entry_id}` - Delete an entry (409 for the only entry). Returns the set

While an On entry is active, the scheduler sets its `target_temp` on each climate (after turning it on).
Off stays off, with no temperature set. Boost uses the active On entry's target, or `default_target_temp`
(stored in `schedule_sets.json`, 20 °C unless changed there) when nothing On is scheduled. On entries
saved before targets existed get `default_target_temp` when loaded.

### Zones
Each Home Assistant Area with managed climate entities is a zone (found through HA's `/api/template` endpoint;
debug mode uses mock areas). Zones are stored in `data/zones.json` and keep their ids across refreshes and restarts.
Each managed entity is in exactly one zone: a manual zone first, else its area's zone, else "Whole house". Each zone
follows its own schedule set, or the active set when `schedule_set_id` is null (the default). A schedule set that a
zone follows can't be deleted (409 naming the zones).

- `GET /zones` - All zones
- `POST /zones/refresh` - Fetch areas from Home Assistant again and rebuild the zones (502 if HA can't be reached)
- `PATCH /zones/{id}` - `{"name": "Office", "schedule_set_id": "<set id>"}`; both optional, `null` set id follows the active set
- `POST /zones/merge` - Merge area zones into the first: `{"zone_ids": ["<id>", "<id>"], "name": "Downstairs"}`
- `POST /zones` - Manual zone: `{"name": "Loft", "entity_ids": ["climate.loft"]}` (its entities leave their area zones)
- `DELETE /zones/{id}` - Delete a manual or merged zone; its entities fall back to their area zones or Whole house

### Weather
With "weather adjust" on, a zone's On target is adjusted for the weather from a Home Assistant weather entity
(debug mode uses mock weather). The entity is read once per scheduler tick; if a read fails, the last good reading is
used for up to 30 minutes, and after that targets aren't adjusted.
It uses the entity's `temperature`, `wind_speed` and `cloud_coverage` attributes (converting °F, m/s, mph and
knots; without `cloud_coverage`, the condition such as `sunny` or `cloudy` gives a rough cloud cover).

- Cold: from 0 at 5 °C outside up to +1.5 °C at -5 °C
- Wind: from 0 at 20 km/h up to the zone's exposure maximum at 50 km/h (low: none, medium: +0.5 °C, high: +1 °C)
- Sun: inside one of the zone's sun windows, from -1.5 °C under clear skies to none at 30 % cloud

The result is clamped to 5-30 °C and to the climate's own min/max, and rounded to 0.5 °C. Once set, a zone's adjusted
target is held until the unrounded value moves at least 0.5 °C from it (0.25 °C past the rounding boundary), so an
outside temperature wobbling around a boundary doesn't re-command TRVs. Off periods are never adjusted.

Early start: with weather adjust on, a zone's next On period starts up to `max_early_start_minutes` early (default
30, 0 disables) when it's cold or windy: none at 10 °C or warmer outside, the full amount at -2 °C or colder, with
wind adding up to a quarter (medium exposure) or half (high) of it. The zone then heats to that period's (adjusted)
target. Once an early start begins it carries on until the period starts, even if it warms up meanwhile, so the
heating isn't turned off and on again.

Late finish: with weather adjust on and `max_late_finish_minutes` set (default 0, off), an On period keeps heating up to
that many minutes past its end on cold or windy days, on the same scale as early start, at the ended period's
(adjusted) target. It's decided when the period ends and then carries on for those minutes whatever the weather does,
and the heating turns off once, when it's over. It only extends into the Off that follows; if it meets the next
period's early start, the early start takes over with no gap. Start times never move.

Cold-day warm-ups: a zone can opt in (`cold_warmups`) to short warm-ups partway through a long Off gap when it's very
cold, so the house doesn't get too cold while nobody's heating it. In an Off gap longer than `min_gap_minutes`, a
warm-up of `burst_minutes` falls `every_minutes` after the gap starts, and again every `every_minutes`, whenever it's
colder than `below_c` outside at that moment. The weather is checked once, as each warm-up starts, and the warm-up
then runs its full length. It heats to the schedule's `default_target_temp`, not adjusted for the weather. A warm-up
that could overlap the zone's late finish after the previous On period, or its early start before the next one, is
skipped, and with no weather reading there's none. Zones that don't opt in stay off for the whole Off gap; nothing
else turns heating on during an Off period.

- `GET /weather` - The configured weather entity, the last reading and any error
- `PUT /weather` - Choose the weather entity: `{"entity_id": "weather.home"}` (`null` to stop); stored in `data/weather.json`
- `POST /weather/mock` - Debug builds only: set the mock weather, e.g. `{"temperature": -2, "wind_speed": 40, "cloud_coverage": 10}`
- `PATCH /zones/{id}` also takes `sun_windows` (`[{"start": "08:00", "end": "11:00"}]`), `wind_exposure`
  (`low`/`medium`/`high`), `weather_adjust` (`true`/`false`), `max_early_start_minutes` and
  `max_late_finish_minutes` (both 0-180), and `cold_warmups`: `{"enabled": true, "below_c": 0, "min_gap_minutes": 240,
  "burst_minutes": 20, "every_minutes": 180}` (the defaults, apart from `enabled`, which is false; fields left out get
  them). The gap and interval must be 1-1440 minutes, the burst 1-180 and shorter than the interval, and `below_c`
  -30 to 20 °C
- `GET /zones` includes each zone's `status`: `state`, `scheduled_target`, adjusted `target`, `reasons`,
  `raw_target` (before clamping, rounding and holding), `held` and `early_start` (`period_start`, `lead_minutes`
  allowed by the weather, `minutes_early` it actually started, and `causes`) while a period is being started early, and
  `late_finish` (`period_end`, `extra_minutes`, `causes`) while one is kept going past its end, and `warm_up`
  (`started_at`, `until`, `outside_c`) during a cold-day warm-up

### Schedule (active set)
- `GET /schedule` - Get the active schedule
- `POST /schedule` - Add an entry to the active schedule
- `DELETE /schedule/{id}` - Delete an entry from the active schedule (409 for the only entry)

### Entities
- `GET /entities` - List all entities with status
- `POST /entities` - Add entities: `{"entity_ids": ["climate.living_room"]}`
- `DELETE /entities` - Remove entity: `{"entity_id": "climate.living_room"}`

### Dry run
- `GET /dry_run` - `{"dry_run": true|false, "calls": [...]}`: the calls a dry run would have made, newest first. Each
  has `summary`, `context` (why), `method`, `endpoint`, `body`, `at`, `last_at` and `repeats`

### Boost
- `POST /boost_all` - Boost all entities (45 min)
- `POST /boost` - Boost specific entities: `{"climate_names": ["climate.living_room"], "time_length": 30}`

## Running

Debug mode (mock entities):
```bash
cargo run
```

Production mode:
```bash
cargo run --release
```

## Try it locally (fake data)

One command runs the debug backend (mock climates, mock areas and mock weather) and the panel's dev server, so you
can try zones and weather without a Home Assistant. It needs Rust, Node 24 with corepack (`corepack enable` once if
`corepack pnpm` isn't found), curl, and the panel cloned next to this repo as `../ha-heating-scheduler-panel` (or
set `PANEL_DIR`).

```bash
scripts/demo.sh
```

It demos this clone's current branch, with the panel branch of the same name if there is one, else panel `main`;
`scripts/demo.sh <backend-ref> <panel-ref>` picks others (`--help` has the details). It works in throwaway git
worktrees and an empty data folder, so your checkouts and any real `data/` or `.env` are left alone, and nothing
talks to a real Home Assistant. Open the panel URL it prints and follow its "what to try" list; Ctrl-C stops both
and cleans up. It needs ports 3000 and 5173 free (`PANEL_PORT` changes the panel's).

## Dry run against your Home Assistant

Dry run reads your Home Assistant (areas, climates and weather) and works out exactly what it would do, without
changing anything. Use it to check zones and weather adjustment on your own house before trusting them.

Every request that would change Home Assistant (`/api/services/...`) is cut at the HA client: it's recorded instead
of sent, and the scheduler carries on as if HA had accepted it. Only reads reach HA: `GET /api/states/...` and
`POST /api/template`, which renders a template. Boost and anything you do in the panel go through the same client.

1. Build the release version (debug builds use mock climates): `cargo build --release`.
2. Use a separate, empty data folder, so the dry run doesn't share state with your real instance. Your normal
   instance keeps port 3000, so give the dry run another port. From the folder that has your `.env`:

   ```bash
   mkdir -p ~/heating-dry-run
   DRY_RUN=1 PORT=3100 DATA_PATH=~/heating-dry-run target/release/ha-heating-scheduler
   ```

   `--dry-run` works too, and wins over any `DRY_RUN`. An unrecognised `DRY_RUN` value, or any other command-line
   argument (a mistyped `--dryrun`, say), refuses to start. Set `CLIMATE_ENTITY` the same way as in your `.env` the first time, since the new data
   folder has no `entities.json` yet. If you'd rather keep port 3000, stop your normal instance first.
3. Before trusting it, check that it's really a dry run:
   - the startup log says `=== DRY RUN: NOT CONTROLLING THE HEATING ===`;
   - `curl localhost:3100/dry_run` answers `{"dry_run": true, ...}`;
   - the panel shows a "DRY RUN" banner. For the dev panel, run it with `VITE_API_BASE_URL=http://localhost:3100`.
4. Watch what it would do: `GET /dry_run` lists the would-be calls, newest first (up to 200), e.g. "would set
   climate.lounge_trv to 21.5 °C" with why: "Lounge: 20 °C scheduled, +1.5 cold = 21.5 °C". Your real heating doesn't
   change, so the scheduler keeps asking for the same thing every tick; a repeat only counts in `repeats`, so the
   list has one entry per change. The panel shows the same list.

Stop it with Ctrl-C. Nothing it did needs undoing.

## Testing

```bash
cargo test
```
