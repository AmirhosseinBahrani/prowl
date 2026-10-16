//! PRowl daemon: watches GitHub for review requests and reviews them with the local `claude` CLI.

pub mod client;
pub mod config;
pub mod daemon;
pub mod events;
mod server;
mod store;
pub mod transport;

pub use client::ApiClient;
pub use config::Config;
pub use daemon::{RunningDaemon, start};
pub use events::Event;
pub use server::Status;
