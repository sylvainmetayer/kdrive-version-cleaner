pub mod api;
pub mod app;
pub mod cleaner;
pub mod config;
pub mod db;
pub mod events;
pub mod rules;
pub mod scanner;
pub mod ui;

/// Horodatage Unix courant, en secondes.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
