//! Keep integration-test ownership metadata out of the user's configuration.
use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

static CONFIG_ENV: Mutex<()> = Mutex::new(());

pub struct IsolatedConfig {
    previous: Option<OsString>,
    _directory: tempfile::TempDir,
    _guard: MutexGuard<'static, ()>,
}

pub fn isolated_config() -> IsolatedConfig {
    // Each integration test has its own runtime, but environment variables
    // are shared by the binary. Serialize their temporary configuration roots.
    let guard = CONFIG_ENV.lock().unwrap_or_else(|error| error.into_inner());
    let directory = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("DEVFLOW_CONFIG_DIR");
    std::env::set_var("DEVFLOW_CONFIG_DIR", directory.path());
    IsolatedConfig {
        previous,
        _directory: directory,
        _guard: guard,
    }
}

impl Drop for IsolatedConfig {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            std::env::set_var("DEVFLOW_CONFIG_DIR", previous);
        } else {
            std::env::remove_var("DEVFLOW_CONFIG_DIR");
        }
    }
}
