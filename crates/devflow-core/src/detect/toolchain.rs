//! mise configuration and language version files.
use super::facts::{MiseEnvEntry, MiseFacts, ToolchainFacts, VersionFile};
use super::manifest::parse_toml;
use super::push_unique;
use super::repo::{Repo, join};
use std::collections::BTreeSet;

// -- how mise layers its configuration files -------------------------------------
//
// One directory's files, lowest precedence first (a later file wins over an
// earlier one), as `mise config ls` lists them (highest first) in mise
// 2026.10; checked with `MISE_ENV=dev` and every name below present:
//
//   .tool-versions
//   .config/mise/conf.d/*.toml                (by name)
//   BASE   .config/mise/config.toml  .config/mise.toml  .mise/config.toml
//          mise/config.toml  mise.toml  .mise.toml
//   ENV    <the same stems with `.<env>` before `.toml`; mise.<env>.toml sits
//          below .mise/config.<env>.toml, unlike the base files>
//   LOCAL  <the BASE stems with `.local` before `.toml`>
//   ENV + LOCAL  <the ENV stems with `.<env>.local`>
//
// A `mise.<env>.toml` is read only when `MISE_ENV` selects `<env>`.

/// The stems of the base and `.local` files, lowest precedence first.
const BASE_STEMS: [&str; 6] = [
    ".config/mise/config",
    ".config/mise",
    ".mise/config",
    "mise/config",
    "mise",
    ".mise",
];
/// The stems of the `.<env>` files, lowest precedence first.
const ENV_STEMS: [&str; 6] = [
    ".config/mise/config",
    ".config/mise",
    "mise/config",
    "mise",
    ".mise/config",
    ".mise",
];
/// Directories that can hold a mise configuration file.
const MISE_CONFIG_DIRS: [&str; 6] = [
    ".",
    ".config",
    ".config/mise",
    ".config/mise/conf.d",
    ".mise",
    "mise",
];
const TOOL_VERSIONS: &str = ".tool-versions";
const CONF_D: &str = ".config/mise/conf.d/";

/// Where a mise configuration file sits among the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MiseLayer {
    /// Higher wins. Files of the same rank (the files of `conf.d`, the same
    /// stem under different environments) are ordered by name.
    rank: u32,
    /// The `MISE_ENV` whose file this is.
    env: Option<String>,
}

fn valid_env(env: &str) -> bool {
    !env.is_empty() && !env.contains('/') && !env.split('.').any(|part| part == "local")
}

/// Classify a repository-relative path as a mise configuration file.
fn mise_layer(path: &str) -> Option<MiseLayer> {
    let layer = |rank: u32, env: Option<&str>| {
        Some(MiseLayer {
            rank,
            env: env.map(str::to_string),
        })
    };
    if path == TOOL_VERSIONS {
        return layer(10, None);
    }
    if path.strip_prefix(CONF_D).is_some_and(|name| {
        name.ends_with(".toml") && !name.contains('/') && name.len() > ".toml".len()
    }) {
        return layer(20, None);
    }
    for (index, stem) in BASE_STEMS.iter().enumerate() {
        if path.strip_prefix(stem) == Some(".toml") {
            return layer(100 + index as u32, None);
        }
        if path.strip_prefix(stem) == Some(".local.toml") {
            return layer(300 + index as u32, None);
        }
    }
    for (index, stem) in ENV_STEMS.iter().enumerate() {
        let Some(rest) = path.strip_prefix(stem).and_then(|r| r.strip_prefix('.')) else {
            continue;
        };
        if let Some(env) = rest.strip_suffix(".local.toml")
            && valid_env(env)
        {
            return layer(400 + index as u32, Some(env));
        }
        if let Some(env) = rest.strip_suffix(".toml")
            && valid_env(env)
        {
            return layer(200 + index as u32, Some(env));
        }
    }
    None
}

/// The precedence rank of a mise configuration file, `None` for a path that
/// is not one. Higher wins; used to ask what mise applies over a file.
pub(super) fn layer_rank(path: &str) -> Option<u32> {
    mise_layer(path).map(|layer| layer.rank)
}

/// Whether a repository-relative path is one of the files mise reads.
pub(super) fn is_layer(path: &str) -> bool {
    mise_layer(path).is_some()
}

/// The mise configuration files of the repository root, in precedence order
/// (lowest first), with the environment their name selects.
pub(super) fn mise_files(repo: &Repo) -> Vec<(String, MiseLayer)> {
    let mut found: Vec<(String, MiseLayer)> = Vec::new();
    for dir in MISE_CONFIG_DIRS {
        for name in repo.list(dir).files() {
            let path = join(dir, name);
            if let Some(layer) = mise_layer(&path) {
                found.push((path, layer));
            }
        }
    }
    found.sort_by(|(a_path, a), (b_path, b)| {
        (a.rank, &a.env, a_path).cmp(&(b.rank, &b.env, b_path))
    });
    found
}
/// Directories mise reads executable file tasks from.
const MISE_TASK_DIRS: [&str; 5] = [
    "mise-tasks",
    ".mise-tasks",
    ".mise/tasks",
    "mise/tasks",
    ".config/mise/tasks",
];
const MAX_TASKS: usize = 200;

pub(super) fn detect(repo: &Repo) -> ToolchainFacts {
    ToolchainFacts {
        mise: mise(repo),
        version_files: version_files(repo),
    }
}

fn mise(repo: &Repo) -> Option<MiseFacts> {
    let mut facts = MiseFacts::default();
    let mut tools = BTreeSet::new();
    let mut tasks = BTreeSet::new();

    // In precedence order, so `env_entries` come out in the order mise applies
    // them.
    for (file, layer) in mise_files(repo) {
        let file = file.as_str();
        facts.config_files.push(file.to_string());
        facts.evidence.push(file.to_string());
        let Some(text) = repo.read_text(file) else {
            continue;
        };
        if file == TOOL_VERSIONS {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if line.is_empty() {
                    continue;
                }
                match tool_versions_entry(line) {
                    Some(tool) => {
                        tools.insert(tool.to_string());
                    }
                    // The line itself is never quoted: it may be a secret.
                    None => repo.warn(
                        file,
                        "contains a line that is not a tool and versions; ignored",
                    ),
                }
            }
            continue;
        }
        let Some(table) = parse_toml(repo, file, &text) else {
            continue;
        };
        if let Some(t) = table.get("tools").and_then(toml::Value::as_table) {
            tools.extend(t.keys().cloned());
        }
        if let Some(t) = table.get("tasks").and_then(toml::Value::as_table) {
            for name in t.keys() {
                tasks.insert(name.clone());
                facts.evidence.push(format!("{file}:tasks.{name}"));
            }
        }
        for entry in env_entries(&text, file, layer.env.as_deref()) {
            push_unique(
                &mut facts.evidence,
                format!("{file}:env._.{}", entry.directive),
            );
            // Every entry counts, repeated ones included: listing a file
            // twice applies it twice, and the second time it wins again.
            facts.env_files.push(entry.path.clone());
            facts.env_entries.push(entry);
        }
    }

    for dir in MISE_TASK_DIRS {
        if repo.is_dir(dir) {
            let before = tasks.len();
            collect_file_tasks(repo, dir, "", &mut tasks);
            if tasks.len() > before {
                facts.evidence.push(dir.to_string());
            }
        }
    }

    if facts.config_files.is_empty() && tasks.is_empty() {
        return None;
    }
    facts.tools = tools.into_iter().collect();
    facts.tasks = tasks.into_iter().take(MAX_TASKS).collect();
    Some(facts)
}

/// The `[env] _.file` / `_.source` entries of one mise file, in the order the
/// file lists them. `[env]` is a table or (`[[env]]`) an array of tables,
/// applied in order; within a table `_.file` and `_.source` keep the order they
/// are written in, which a sorted map would lose.
fn env_entries(text: &str, file: &str, env: Option<&str>) -> Vec<MiseEnvEntry> {
    use toml_edit::TableLike;
    let Ok(document) = text.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    let Some(section) = document.get("env") else {
        return Vec::new();
    };
    let sections: Vec<&dyn TableLike> = if let Some(table) = section.as_table_like() {
        vec![table]
    } else if let Some(tables) = section.as_array_of_tables() {
        tables.iter().map(|t| t as &dyn TableLike).collect()
    } else {
        Vec::new()
    };
    let mut entries = Vec::new();
    for section in sections {
        let Some(directives) = section.get("_").and_then(toml_edit::Item::as_table_like) else {
            continue;
        };
        for (directive, item) in directives.iter() {
            if directive != "file" && directive != "source" {
                continue;
            }
            let mut paths = Vec::new();
            collect_paths(item, 0, &mut paths);
            entries.extend(paths.into_iter().map(|path| MiseEnvEntry {
                file: file.to_string(),
                directive: directive.to_string(),
                path,
                env: env.map(str::to_string),
            }));
        }
    }
    entries
}

/// File paths in a mise `_.file`/`_.source` value: a string, a table with
/// `path`, or an array of either.
fn collect_paths(item: &toml_edit::Item, depth: usize, out: &mut Vec<String>) {
    if depth > 3 {
        return;
    }
    if let Some(path) = item.as_str() {
        out.push(path.to_string());
    } else if let Some(items) = item.as_array() {
        for value in items {
            collect_value_paths(value, depth + 1, out);
        }
    } else if let Some(tables) = item.as_array_of_tables() {
        for table in tables {
            if let Some(path) = table.get("path") {
                collect_paths(path, depth + 1, out);
            }
        }
    } else if let Some(table) = item.as_table_like()
        && let Some(path) = table.get("path")
    {
        collect_paths(path, depth + 1, out);
    }
}

/// A path in an array: a string, or an inline table with a `path`.
fn collect_value_paths(value: &toml_edit::Value, depth: usize, out: &mut Vec<String>) {
    if depth > 3 {
        return;
    }
    if let Some(path) = value.as_str() {
        out.push(path.to_string());
    } else if let Some(table) = value.as_inline_table()
        && let Some(path) = table.get("path")
    {
        collect_value_paths(path, depth + 1, out);
    } else if let Some(items) = value.as_array() {
        for item in items {
            collect_value_paths(item, depth + 1, out);
        }
    }
}

/// Executable files under a task directory; nested directories join with `:`.
fn collect_file_tasks(repo: &Repo, dir: &str, prefix: &str, out: &mut BTreeSet<String>) {
    let index = repo.list(dir);
    for name in index.files().filter(|n| !n.starts_with('.')) {
        out.insert(format!("{prefix}{name}"));
    }
    if prefix.is_empty() {
        for sub in index.dirs().filter(|n| !n.starts_with('.')).take(20) {
            collect_file_tasks(repo, &join(dir, sub), &format!("{sub}:"), out);
        }
    }
}

fn version_files(repo: &Repo) -> Vec<VersionFile> {
    const FILES: [(&str, &str); 6] = [
        (".nvmrc", "node"),
        (".node-version", "node"),
        (".python-version", "python"),
        (".ruby-version", "ruby"),
        ("rust-toolchain.toml", "rust"),
        ("rust-toolchain", "rust"),
    ];
    let root = repo.list(".");
    FILES
        .iter()
        .filter(|(file, _)| root.has_file(file))
        .map(|(file, tool)| VersionFile {
            file: (*file).to_string(),
            tool: (*tool).to_string(),
            version: read_version(repo, file),
        })
        .collect()
}

/// The version a pin file names.
///
/// The text comes from the general reader, which refuses dotenv files and
/// credential stores even when the pin file is a link to one. As a second
/// layer the value must be a single version-like token (`22`, `lts/iron`,
/// `3.12.4`, `nightly-2024-05-01`; see [`is_version_token`]): anything else,
/// such as a connection string or a `KEY value` pair, is dropped, with a
/// warning that does not repeat it.
fn read_version(repo: &Repo, file: &str) -> Option<String> {
    let text = repo.read_head(file, 4096)?;
    let value = if file == "rust-toolchain.toml" {
        let table = parse_toml(repo, file, &text)?;
        table
            .get("toolchain")
            .and_then(|t| t.get("channel"))
            .and_then(toml::Value::as_str)
            .map(str::to_string)?
    } else {
        // Plain files: first line that is not blank or a comment.
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))?
            .to_string()
    };
    if is_version_token(value.trim()) {
        Some(value.trim().to_string())
    } else {
        repo.warn(file, "does not contain a version; ignored");
        None
    }
}

/// One version-like word: starts with a letter or digit, then letters,
/// digits and `. _ + ~ / * -`, at most 64 bytes, nothing else. No `=`, `:`
/// or `@` (connection strings, `KEY=value`, `user@host`) and no whitespace
/// (a second word is not part of a version).
fn is_version_token(token: &str) -> bool {
    let mut bytes = token.bytes();
    token.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'~' | b'/' | b'*' | b'-')
        })
}

/// The tool a `.tool-versions` line names, if the line is `tool version...`:
/// a tool name (a version-like word, optionally `backend:name` as in
/// `npm:prettier`) and one or more versions (version-like words, or mise's
/// `ref:` and `prefix:` forms). Anything else is not such a line.
fn tool_versions_entry(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    let tool = words.next()?;
    let named = match tool.split_once(':') {
        Some((backend, name)) => is_version_token(backend) && is_version_token(name),
        None => is_version_token(tool),
    };
    let mut versions = 0;
    for word in words {
        let version = word
            .strip_prefix("ref:")
            .or_else(|| word.strip_prefix("prefix:"))
            .unwrap_or(word);
        if !is_version_token(version) {
            return None;
        }
        versions += 1;
    }
    (named && versions > 0).then_some(tool)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Every name mise reads, lowest precedence first: the order `mise config
    /// ls` printed (reversed) for mise 2026.10.7 with `MISE_ENV=dev` and all
    /// of these files present.
    pub(crate) const VERIFIED_ORDER: [&str; 27] = [
        ".tool-versions",
        ".config/mise/conf.d/a.toml",
        ".config/mise/conf.d/b.toml",
        ".config/mise/config.toml",
        ".config/mise.toml",
        ".mise/config.toml",
        "mise/config.toml",
        "mise.toml",
        ".mise.toml",
        ".config/mise/config.dev.toml",
        ".config/mise.dev.toml",
        "mise/config.dev.toml",
        "mise.dev.toml",
        ".mise/config.dev.toml",
        ".mise.dev.toml",
        ".config/mise/config.local.toml",
        ".config/mise.local.toml",
        ".mise/config.local.toml",
        "mise/config.local.toml",
        "mise.local.toml",
        ".mise.local.toml",
        ".config/mise/config.dev.local.toml",
        ".config/mise.dev.local.toml",
        "mise/config.dev.local.toml",
        "mise.dev.local.toml",
        ".mise/config.dev.local.toml",
        ".mise.dev.local.toml",
    ];

    #[test]
    fn layers_rank_as_mise_orders_them() {
        let ranks: Vec<u32> = VERIFIED_ORDER
            .iter()
            .map(|path| layer_rank(path).unwrap_or_else(|| panic!("{path} is a mise file")))
            .collect();
        // The files of `conf.d` share a rank and are ordered by name.
        for (index, pair) in ranks.windows(2).enumerate() {
            let same_directory = VERIFIED_ORDER[index].starts_with(CONF_D)
                && VERIFIED_ORDER[index + 1].starts_with(CONF_D);
            assert!(
                pair[0] < pair[1] || (same_directory && pair[0] == pair[1]),
                "{} must rank below {}",
                VERIFIED_ORDER[index],
                VERIFIED_ORDER[index + 1]
            );
        }
        let layer = mise_layer("mise.production.local.toml").unwrap();
        assert_eq!(layer.env.as_deref(), Some("production"));
        assert_eq!(mise_layer("mise.toml").unwrap().env, None);
    }

    #[test]
    fn other_files_are_not_mise_configuration() {
        for path in [
            "package.json",
            "mise.toml.bak",
            "xmise.toml",
            "mise.local.dev.toml",
            "mise..toml",
            "sub/mise.toml",
            ".config/mise/conf.d/nested/x.toml",
            ".config/mise/conf.d/.toml",
            "mise-tasks.toml",
        ] {
            assert_eq!(layer_rank(path), None, "{path}");
        }
    }
}
