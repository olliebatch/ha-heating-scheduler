# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

A Rust-based Home Assistant heating scheduler that manages climate entities through scheduled periods and temporary boosts. The system runs continuously, polling every 15 seconds to apply heating schedules and boost overrides to Home Assistant climate entities.

## Build, Test, and Run Commands

```bash
# Build (debug mode with mock climate entities)
cargo build

# Build (production mode with real Home Assistant API calls)
cargo build --release

# Run tests
cargo test

# Run in debug mode (uses MockClimate, no real API calls)
cargo run

# Run in production mode (uses DefaultClimate with real Home Assistant API)
cargo run --release

# Run a single test
cargo test test_name

# Run tests with output
cargo test -- --nocapture
```

## Configuration

Create a `.env` file with:
- `HA_URL`: Home Assistant URL (e.g., `http://homeassistant:8123`)
- `HA_TOKEN`: Long-lived access token from Home Assistant
- `DATA_PATH`: Directory for persistent data (e.g., `./data`)

Entities are persisted in `data/entities.json` (managed via API, not env vars after initial migration).

## Architecture

### Core Modules

**Climate Module** (`src/climate/`)
- `ClimateEntity` trait: Defines interface for climate entity operations (fetch state, turn on/off, boost management)
- `ClimateEntityWrapper` enum: Allows runtime switching between `MockClimate` (debug) and `DefaultClimate` (production)
- `MockClimate`: Debug implementation that simulates climate entities without API calls
- `DefaultClimate`: Production implementation that makes real Home Assistant API calls via `climate/set_hvac_mode` service
- `ClimateInfo`: Current state snapshot (temperature, heating on/off)
- `BoostInfo`: Temporary override with start/end times

**Schedule Module** (`src/schedule/`)
- `Schedule`: Container for schedule entries with logic to determine current desired state
- `ScheduleEntry`: Time period + heating state (on/off) with UUID for deletion
- `TimePeriod`: Start/end times supporting midnight-crossing periods (e.g., 22:00-06:00)
- Key feature: Adding entries automatically splits overlapping periods to maintain full day coverage
- Deletion extends previous entry to fill gaps
- Persistence via JSON in `data/schedule.json`

**Scheduler Module** (`src/scheduler/`)
- Main loop runs every 15 seconds
- Calculates desired heating state from schedule
- Checks for active boosts and removes expired ones
- Combines schedule state + boost state (boost takes precedence)
- Compares with current state and applies changes via Home Assistant API
- Uses `RwLock` to manage shared state across threads

**Server Module** (`src/server/`)
- Axum REST API on port 3000
- CORS enabled for frontend integration
- Endpoints:
  - `/schedule` (GET/POST): Manage schedule entries
  - `/schedule/{id}` (DELETE): Remove entry by UUID
  - `/entities` (GET/POST/DELETE): Manage climate entities
  - `/boost` and `/boost_all`: Temporary heating overrides

**API Client** (`src/api_client.rs`)
- Wrapper around Home Assistant REST API
- Handles authentication (Bearer token)
- Fetches climate state from `/api/states/{entity_id}`
- Controls climate via `/api/services/climate/set_hvac_mode`

### Key Design Patterns

**Debug vs Production Mode**
- Controlled by `#[cfg(debug_assertions)]` and `#[cfg(not(debug_assertions))]`
- Debug mode uses `MockClimate` for development without Home Assistant
- Production mode uses `DefaultClimate` for real Home Assistant control
- main.rs:33-50 shows the conditional initialization

**Concurrency Model**
- Two main async tasks: API server and scheduler loop
- `Arc<RwLock<T>>` for shared state (schedule, climate entities)
- Clone entities before processing to avoid holding locks across await points (scheduler/mod.rs:111-114)
- Update shared state after processing batch (scheduler/mod.rs:152-154)

**Schedule Time Period Logic**
- Full day coverage maintained by automatic splitting on overlap
- Midnight-crossing periods supported (schedule/mod.rs:32-44)
- Deletion extends previous entry to avoid gaps (schedule/mod.rs:335-374)
- Complex subtraction logic handles period arithmetic (schedule/mod.rs:58-202)

**Boost System**
- Temporary heating override stored per-entity in `BoostInfo`
- Checked each scheduler tick to see if boost is expired
- Boost state combined with schedule state where boost takes priority (scheduler/mod.rs:58-71)
- Expired boosts automatically removed (scheduler/mod.rs:122-126)

## Development Notes

- When adding new climate entity fields, update both `MockClimate` and `DefaultClimate` to maintain parallel implementations
- Schedule tests extensively validate time period arithmetic including midnight crossing - run these when modifying TimePeriod logic
- The scheduler loop clones entities to process outside locks - this pattern must be maintained to avoid deadlocks
- UUID tracking for schedule entries enables frontend to reliably delete/update specific entries
- Home Assistant API expects `hvac_mode` values: "off" or "heat" (see climate/mod.rs:143-171)
