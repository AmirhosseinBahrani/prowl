//! `prowl`: the terminal UI for the PRowl daemon. It talks to `prowld` over its Local API, so
//! it works the same on the desktop and over SSH on an always-on server.

pub mod app;
pub mod policy_tab;
mod policy_ui;
pub mod runtime;
pub mod ui;

pub use runtime::{Editor, Options, run};
