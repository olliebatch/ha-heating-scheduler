# Home Assistant Heating Scheduler

A work in progress to build a more complex home assistant heating schedule that allows easy control over the schedules and extra functionalities easily accessible.

## Configuration

Create a `.env` file:

```env
HA_URL=http://your-home-assistant:8123
HA_TOKEN=your-long-lived-access-token
DATA_PATH=./data
```

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
target. End times never move, and nothing else turns heating on during an Off period.

- `GET /weather` - The configured weather entity, the last reading and any error
- `PUT /weather` - Choose the weather entity: `{"entity_id": "weather.home"}` (`null` to stop); stored in `data/weather.json`
- `POST /weather/mock` - Debug builds only: set the mock weather, e.g. `{"temperature": -2, "wind_speed": 40, "cloud_coverage": 10}`
- `PATCH /zones/{id}` also takes `sun_windows` (`[{"start": "08:00", "end": "11:00"}]`), `wind_exposure`
  (`low`/`medium`/`high`), `weather_adjust` (`true`/`false`) and `max_early_start_minutes` (0-180)
- `GET /zones` includes each zone's `status`: `state`, `scheduled_target`, adjusted `target`, `reasons`,
  `raw_target` (before clamping, rounding and holding), `held` and `early_start` (`period_start`, `lead_minutes`,
  `causes`) while a period is being started early

### Schedule (active set)
- `GET /schedule` - Get the active schedule
- `POST /schedule` - Add an entry to the active schedule
- `DELETE /schedule/{id}` - Delete an entry from the active schedule

### Entities
- `GET /entities` - List all entities with status
- `POST /entities` - Add entities: `{"entity_ids": ["climate.living_room"]}`
- `DELETE /entities` - Remove entity: `{"entity_id": "climate.living_room"}`

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

## Testing

```bash
cargo test
```
