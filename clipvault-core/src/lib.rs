//! clipvault-core: headless clipboard-history core.
//!
//! No GTK dependency — everything here is unit-testable without a display.

pub mod backend;
pub mod config;
pub mod filter;
pub mod storage;
pub mod types;
