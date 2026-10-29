//! The agent core that every `ox` frontend drives: configuration, the model client, tools,
//! permissions, sessions, and the [`engine::Engine`] behind the [`protocol`] contract.

pub mod auth;
pub mod client;
pub mod command;
pub mod config;
pub mod engine;
mod file_tracker;
pub mod git;
mod message;
pub mod model;
pub mod permission;
mod prompt;
pub mod protocol;
pub mod session;
pub mod tool;
pub mod util;
