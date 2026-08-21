//! UI panes. Each is an `impl crate::app::App` block in its own file so panes
//! can borrow whatever App state they need without cross-module plumbing.

mod connect;
mod log;
pub use log::wrap_len;

mod plot;
mod settings;
mod transmit;
mod update;
mod windows;
