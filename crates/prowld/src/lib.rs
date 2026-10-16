//! PRowl daemon: watches GitHub for review requests and reviews them with the local `claude` CLI.

pub mod auth;
mod claude;
pub mod client;
pub mod config;
mod credentials;
pub mod daemon;
pub mod events;
pub mod gate;
mod github;
pub mod jobs;
pub mod policy;
mod policy_api;
pub mod poller;
mod prompt;
pub mod repos;
mod repos_api;
pub mod review;
pub mod reviews;
mod server;
pub mod service;
mod store;
pub mod transport;
mod worker;
mod workspace;

pub use auth::AuthState;
pub use client::ApiClient;
pub use config::{Config, CredentialBackend};
pub use daemon::{RunningDaemon, start};
pub use events::Event;
pub use jobs::Job;
pub use policy::{EffectivePolicy, PolicyRule, PolicyVersion};
pub use poller::PollSummary;
pub use repos::{Repo, RepoPage, RepoQuery};
pub use review::ReviewOutput;
pub use reviews::Review;
pub use server::Status;
