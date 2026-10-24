//! devflow core: Git worktrees with copy-on-write payloads, Postgres branches
//! cloned with `file_copy_method = clone`, and trusted setup hooks.
//!
//! The API is synchronous. Frontends (CLI, GUI) call it directly or from their
//! own background executors; no async runtime or UI type appears here.

pub mod config;
pub mod config_edit;
pub mod conn;
pub mod cow;
pub mod db;
pub mod detect;
pub mod diag;
pub mod doctor;
pub mod env_file;
pub mod env_owner;
pub mod error;
pub mod exec;
pub mod gc;
pub mod githook;
pub mod health;
pub mod hooks;
pub mod ids;
pub mod integrate;
pub mod ledger;
pub mod paths;
pub mod pitchfork;
pub mod pitchfork_scope;
pub mod ports;
pub mod processes;
pub mod project;
pub mod propose;
pub mod receipt;
pub mod redact;
pub mod registry;
pub mod remedy;
pub mod scope;
pub mod services;
pub mod settings;
pub mod setup;
pub mod text;
pub mod trust;
pub mod wait;
pub mod wide;
pub mod worktree;
pub mod worktreeinclude;

mod fsutil;
mod subprocess;

pub use config::Config;
pub use project::Project;
