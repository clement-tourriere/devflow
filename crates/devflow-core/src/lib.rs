//! devflow core: Git worktrees with copy-on-write payloads, Postgres branches
//! cloned with `file_copy_method = clone`, and trusted setup hooks.
//!
//! The API is synchronous. Frontends (CLI, GUI) call it directly or from their
//! own background executors; no async runtime or UI type appears here.

pub mod config;
pub mod conn;
pub mod cow;
pub mod db;
pub mod doctor;
pub mod env_file;
pub mod error;
pub mod githook;
pub mod hooks;
pub mod paths;
pub mod pitchfork;
pub mod project;
pub mod receipt;
pub mod redact;
pub mod registry;
pub mod setup;
pub mod trust;
pub mod worktree;

mod fsutil;

pub use config::Config;
pub use project::Project;
