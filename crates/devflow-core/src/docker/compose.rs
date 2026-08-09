use std::path::Path;

/// Compose files probed in the current directory during `devflow init`
/// service discovery. The caller parses the YAML itself.
pub fn find_docker_compose_files() -> Vec<String> {
    let compose_filenames = vec![
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
        "docker-compose.override.yml",
        "docker-compose.override.yaml",
    ];

    compose_filenames
        .into_iter()
        .filter(|filename| Path::new(filename).exists())
        .map(|s| s.to_string())
        .collect()
}
