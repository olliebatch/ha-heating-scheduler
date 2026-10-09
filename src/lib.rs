use std::sync::{Arc, RwLock};

pub mod api_client;
pub mod climate;
pub mod config;
pub mod schedule;
pub mod server;
pub mod weather;
pub mod zones;

pub mod scheduler;

pub type ScheduleState = Arc<RwLock<schedule::sets::ScheduleSets>>;
pub type ZonesState = Arc<RwLock<zones::Zones>>;
pub type WeatherState = Arc<RwLock<weather::WeatherStatus>>;
