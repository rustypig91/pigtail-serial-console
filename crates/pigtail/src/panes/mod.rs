//! UI panes. Each is an `impl crate::app::App` block in its own file so panes
//! can borrow whatever App state they need without cross-module plumbing.

mod chrome;
#[cfg(test)]
pub(crate) use chrome::app_visuals;
pub(crate) use chrome::settings_visuals;
mod connect;
mod log;
pub use log::wrap_len;

mod macros;
mod plot;
mod screen;
pub(crate) use screen::ScreenSearch;
mod settings;
mod transfer;
mod transmit;
mod update;
mod windows;

mod workspace;
pub(crate) use workspace::Workspace;
