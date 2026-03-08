//! Skills management for devflow.
//!
//! Provides skill discovery, installation, removal, and update management
//! using the Agent Skills open standard (agentskills.io).

pub mod bundled;
pub mod cache;
pub mod installer;
pub mod manifest;
pub mod marketplace;
pub mod types;
pub mod user_installer;

pub use types::*;
