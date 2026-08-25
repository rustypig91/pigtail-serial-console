//! UI-agnostic core for Pigtail, a serial terminal.
//!
//! This crate must never depend on `egui`/`eframe`. It is usable by a headless
//! CLI recorder with no changes. See `README.md` for the full specification.

pub mod ansi;
pub mod clock;
pub mod config;
pub mod enumerate;
pub mod extract;
pub mod filter;
pub mod framer;
pub mod reader;
pub mod series;
pub mod session;
pub mod source;
pub mod store;
pub mod update;
pub mod wake;
