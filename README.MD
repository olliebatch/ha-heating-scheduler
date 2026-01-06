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

### Schedule
- `GET /schedule` - Get current schedule
- `POST /schedule` - Add schedule entry
- `DELETE /schedule/{id}` - Delete schedule entry

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
