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

/// Git fixtures use the same embedded library as production workspace code.
#[allow(dead_code)]
pub fn init_repository(root: &std::path::Path, branch: &str) {
    let repo = git2::Repository::init_opts(
        root,
        git2::RepositoryInitOptions::new().initial_head(branch),
    )
    .unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let tree = repo
        .find_tree(repo.index().unwrap().write_tree().unwrap())
        .unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
        .unwrap();
}

/// Cleanup must also work during unwinding, when the test runtime cannot be
/// re-entered. A Rust thread owns a small runtime for the Docker API request.
#[cfg(feature = "service-local")]
#[allow(dead_code)]
pub fn docker_cleanup(container: &str) {
    let container = container.to_string();
    std::thread::spawn(move || {
        let result: anyhow::Result<()> = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let docker = bollard::Docker::connect_with_local_defaults()?;
                match docker
                    .remove_container(
                        &container,
                        Some(bollard::query_parameters::RemoveContainerOptions {
                            force: true,
                            ..Default::default()
                        }),
                    )
                    .await
                {
                    Ok(())
                    | Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => (),
                    Err(error) => return Err(error.into()),
                }
                match docker
                    .remove_volume(
                        &format!("{container}-data"),
                        None::<bollard::query_parameters::RemoveVolumeOptions>,
                    )
                    .await
                {
                    Ok(())
                    | Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => Ok(()),
                    Err(error) => Err(error.into()),
                }
            });
        if let Err(error) = result {
            eprintln!("Disposable Docker cleanup failed for {container}: {error:#}");
        }
    })
    .join()
    .unwrap();
}
