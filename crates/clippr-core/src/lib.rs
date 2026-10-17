//! Shared types for the clippr daemon, CLI and GUI.
//!
//! This crate is the contract between processes. Changing a serialized type
//! here changes the on-disk config, the sidecar format or the IPC protocol.

pub mod client;
pub mod clip;
pub mod config;
pub mod edit;
pub mod ipc;
pub mod library;
pub mod paths;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
