//! PRowl daemon: watches GitHub for review requests and reviews them with the local `claude` CLI.

pub mod auth;
pub mod client;
pub mod config;
mod credentials;
pub mod daemon;
pub mod events;
mod github;
pub mod policy;
mod policy_api;
mod server;
pub mod service;
mod store;
pub mod transport;

pub use auth::AuthState;
pub use client::ApiClient;
pub use config::{Config, CredentialBackend};
pub use daemon::{RunningDaemon, start};
pub use events::Event;
pub use policy::{EffectivePolicy, PolicyRule, PolicyVersion};
pub use server::Status;
