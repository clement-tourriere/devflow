//! Configuration devflow, pitchfork and mise already have in the repository.
use super::facts::{PitchforkDaemon, PitchforkFacts, ToolchainFacts, WiringFacts};
use super::manifest::parse_toml;
use super::push_unique;
use super::repo::Repo;

/// devflow 1.0 project files, in lookup order.
const DEVFLOW_CONFIGS: [&str; 2] = ["devflow.toml", ".devflow.toml"];
/// Files only devflow 0.x used.
const LEGACY_FILES: [&str; 4] = [
    ".devflow.yml",
    ".devflow.yaml",
    ".devflow.local.yml",
    ".devflow.local.yaml",
];

pub(super) fn detect(repo: &Repo, toolchain: &ToolchainFacts) -> WiringFacts {
    let root = repo.list(".");
    let devflow_config = root.first_file(&DEVFLOW_CONFIGS).map(str::to_string);
    let devflow_local_config = root
        .has_file("devflow.local.toml")
        .then(|| "devflow.local.toml".to_string());
    let legacy_devflow_files: Vec<String> = LEGACY_FILES
        .iter()
        .filter(|f| root.has_file(f))
        .map(|f| (*f).to_string())
        .collect();

    let mut evidence: Vec<String> = Vec::new();
    for file in devflow_config
        .iter()
        .chain(&devflow_local_config)
        .chain(&legacy_devflow_files)
    {
        push_unique(&mut evidence, file.clone());
    }

    let pitchfork = if root.has_file("pitchfork.toml") {
        push_unique(&mut evidence, "pitchfork.toml");
        pitchfork(repo)
    } else {
        None
    };

    let mut mise_loads_devflow_env = false;
    let mut mise_env_unresolved = Vec::new();
    let mut mise_env_overridden_by = Vec::new();
    let mut mise_env_overridden_when_selected = Vec::new();
    if let Some(mise) = &toolchain.mise {
        // The entries in the order mise applies them: layer by layer, lowest
        // precedence first, then as listed, repeated entries included. Entries
        // of `mise.<env>.toml` only count when that environment is selected,
        // so they cannot be what loads `.devflow/env` (it would go missing
        // without the environment), but they sit in the sequence and can
        // override it.
        let mut last_workspace = None;
        for (index, entry) in mise.env_entries.iter().enumerate() {
            if entry.env.is_some() {
                continue;
            }
            match classify_env_path(&entry.path) {
                EnvPath::ThisWorkspace => {
                    mise_loads_devflow_env = true;
                    last_workspace = Some(index);
                }
                EnvPath::Unresolved => push_unique(&mut mise_env_unresolved, entry.path.clone()),
                EnvPath::Unrelated => {}
            }
        }
        if let Some(index) = last_workspace {
            for entry in &mise.env_entries[index + 1..] {
                if entry.env.is_some() {
                    mise_env_overridden_when_selected.push(entry.clone());
                } else {
                    mise_env_overridden_by.push(entry.clone());
                }
            }
        }
        if mise_loads_devflow_env || !mise_env_unresolved.is_empty() {
            for entry in mise.evidence.iter().filter(|e| e.contains(":env._.")) {
                push_unique(&mut evidence, entry.clone());
            }
        }
    }
    WiringFacts {
        devflow_config,
        devflow_local_config,
        legacy_devflow_files,
        pitchfork,
        mise_loads_devflow_env,
        mise_env_unresolved,
        mise_env_overridden_by,
        mise_env_overridden_when_selected,
        evidence,
    }
}

/// Whether a mise `_.file` path loads the `.devflow/env` of the workspace that
/// holds the mise configuration.
pub(crate) fn is_devflow_env_path(path: &str) -> bool {
    classify_env_path(path) == EnvPath::ThisWorkspace
}

/// What a mise `_.file` value is, as far as `.devflow/env` goes.
#[derive(Debug, PartialEq, Eq)]
enum EnvPath {
    /// The `.devflow/env` of the directory holding the mise config.
    ThisWorkspace,
    /// Mentions `.devflow/env` but is anchored somewhere that cannot be shown
    /// to be this workspace.
    Unresolved,
    /// Not about `.devflow/env`.
    Unrelated,
}

/// Classify a mise `_.file` value.
///
/// Only paths anchored to the workspace that holds the mise config count:
/// `.devflow/env`, `./.devflow/env` and `{{config_root}}/.devflow/env`
/// (mise's own variable for that directory). An absolute path names one
/// particular checkout, which is another worktree's env file when the config
/// is shared by every worktree; any other template, `~` or `..` is unknowable
/// here. Those are unresolved, not wired.
fn classify_env_path(path: &str) -> EnvPath {
    const ENV: &str = ".devflow/env";
    let path = path.trim();
    let anchored = if let Some(rest) = path.strip_prefix("{{") {
        rest.split_once("}}")
            .filter(|(variable, _)| variable.trim() == "config_root")
            .and_then(|(_, tail)| tail.strip_prefix('/'))
    } else {
        Some(path.strip_prefix("./").unwrap_or(path))
    };
    if anchored == Some(ENV) {
        EnvPath::ThisWorkspace
    } else if path.contains(ENV) {
        EnvPath::Unresolved
    } else {
        EnvPath::Unrelated
    }
}

fn pitchfork(repo: &Repo) -> Option<PitchforkFacts> {
    let file = "pitchfork.toml";
    let table = parse_toml(repo, file, &repo.read_text(file)?)?;
    let mut daemons: Vec<PitchforkDaemon> = table
        .get("daemons")
        .and_then(toml::Value::as_table)
        .map(|t| {
            t.iter()
                .map(|(name, daemon)| PitchforkDaemon {
                    name: name.clone(),
                    mise: daemon.get("mise").and_then(toml::Value::as_bool) == Some(true),
                })
                .collect()
        })
        .unwrap_or_default();
    daemons.sort_by(|a, b| a.name.cmp(&b.name));
    let all_daemons_use_mise = !daemons.is_empty() && daemons.iter().all(|d| d.mise);
    let evidence = daemons
        .iter()
        .map(|d| format!("{file}:daemons.{}", d.name))
        .collect();
    Some(PitchforkFacts {
        file: file.to_string(),
        daemons,
        all_daemons_use_mise,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_workspace_anchored_paths_are_wired() {
        for path in [
            ".devflow/env",
            "./.devflow/env",
            " .devflow/env ",
            "{{config_root}}/.devflow/env",
            "{{ config_root }}/.devflow/env",
        ] {
            assert_eq!(classify_env_path(path), EnvPath::ThisWorkspace, "{path}");
        }
        for path in [
            "/tmp/main/.devflow/env",
            "~/work/app/.devflow/env",
            "../main/.devflow/env",
            "sub/.devflow/env",
            "x.devflow/env",
            "{{env.HOME}}/app/.devflow/env",
            "{{cwd}}/.devflow/env",
            "{{config_root}}/../main/.devflow/env",
            "{{config_root}}.devflow/env",
            ".devflow/env.bak",
            "{{ env.DEVFLOW_ENV }}/.devflow/env",
        ] {
            assert_eq!(classify_env_path(path), EnvPath::Unresolved, "{path}");
        }
        for path in [".env", "{{config_root}}/.env", "config/env", "{{env.X}}"] {
            assert_eq!(classify_env_path(path), EnvPath::Unrelated, "{path}");
        }
    }
}
