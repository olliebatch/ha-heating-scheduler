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
- `POST /schedule_sets/{id}/entries` - Add an entry to a set (400 if start equals end, except 00:00 - 00:00). Returns the set
- `DELETE /schedule_sets/{id}/entries/{entry_id}` - Delete an entry (409 for the only entry). Returns the set

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
