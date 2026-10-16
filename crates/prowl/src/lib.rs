//! `prowl`: the terminal UI for the PRowl daemon. It talks to `prowld` over its Local API, so
//! it works the same on the desktop and over SSH on an always-on server.

pub mod app;
pub mod cli;
pub mod feed_tab;
mod feed_ui;
pub mod history_tab;
pub mod policy_tab;
pub(crate) mod policy_ui;
pub mod queue_tab;
mod queue_ui;
pub mod runtime;
pub mod settings_tab;
mod settings_ui;
pub mod ui;

pub use runtime::{Editor, Opener, Options, run};
