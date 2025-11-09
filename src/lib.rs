use std::sync::{Arc, RwLock};

pub mod api_client;
pub mod climate;
pub mod config;
pub mod schedule;
pub mod server;

pub mod scheduler;

pub type ScheduleState = Arc<RwLock<schedule::Schedule>>;