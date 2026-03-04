pub mod compose;
#[cfg(feature = "service-local")]
pub mod discovery;

// Re-export compose functions for backward compatibility
pub use compose::*;
#[cfg(feature = "service-local")]
pub use discovery::*;
