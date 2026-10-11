//! Dependency manifests of every ecosystem, reduced to what detectors need,
//! and the discovery of which directories count as projects.
//!
//! Nothing here executes a build tool: manifests are parsed as data
//! (`package.json`, `pyproject.toml`, `Cargo.toml`, `composer.json`) or
//! scanned for dependency names (`Gemfile.lock`, `mix.exs`, `go.mod`,
//! `requirements*.txt`, Gradle and Maven files).
use super::facts::{PackageManager, PackageManagerFact, WorkspaceSource};
use super::repo::{DirIndex, Repo, join};
use super::{globs, js};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// Most project directories one scan inspects.
const MAX_PROJECTS: usize = 300;
/// Directories whose children are looked at one level deeper.
const CONTAINER_DIRS: [&str; 8] = [
    "apps", "packages", "services", "libs", "modules", "projects", "crates", "workers",
];
/// Directory names that never hold a project of their own.
const SKIP_DIRS: [&str; 17] = [
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    "out",
    "deps",
    "_build",
    "venv",
    "tmp",
    "log",
    "logs",
    "coverage",
    "bower_components",
    "site-packages",
    "__pycache__",
    "storage",
];

/// Dependencies declared in one ecosystem's manifests.
#[derive(Debug, Default)]
pub(crate) struct Deps {
    /// Manifest files that exist, relative to the scan root.
    pub files: Vec<String>,
    names: BTreeMap<String, String>,
    features: BTreeMap<String, Vec<String>>,
    /// Tool tables present in the manifest (`tool.uv`, `tool.poetry`).
    markers: Vec<&'static str>,
}

impl Deps {
    fn add_file(&mut self, file: String) {
        if !self.files.contains(&file) {
            self.files.push(file);
        }
    }

    fn add(&mut self, name: &str, evidence: String) {
        self.names.entry(name.to_string()).or_insert(evidence);
    }

    /// Whether the manifest has the given tool table.
    pub(crate) fn has_marker(&self, marker: &str) -> bool {
        self.markers.contains(&marker)
    }

    /// Whether any manifest file of this ecosystem exists.
    pub(crate) fn present(&self) -> bool {
        !self.files.is_empty()
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.names.contains_key(name)
    }

    /// The `file:key` that declares `name`.
    pub(crate) fn evidence(&self, name: &str) -> Option<String> {
        self.names.get(name).cloned()
    }

    /// Dependency names with their evidence.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.names.iter().map(|(n, e)| (n.as_str(), e.as_str()))
    }

    /// The first dependency whose name starts with `prefix` (Go module paths).
    pub(crate) fn with_prefix(&self, prefix: &str) -> Option<(&str, &str)> {
        self.names
            .range(prefix.to_string()..)
            .next()
            .filter(|(n, _)| n.starts_with(prefix))
            .map(|(n, e)| (n.as_str(), e.as_str()))
    }

    /// Cargo features requested for `name`.
    pub(crate) fn features(&self, name: &str) -> &[String] {
        self.features.get(name).map_or(&[], Vec::as_slice)
    }
}

/// `package.json`, minus the dependency lists.
#[derive(Debug)]
pub(crate) struct JsManifest {
    pub file: String,
    pub name: Option<String>,
    pub scripts: BTreeMap<String, String>,
    /// Raw `packageManager` value (`pnpm@9.1.0+sha512...`).
    pub package_manager: Option<String>,
    /// `devEngines.packageManager` as `name` or `name@version`.
    pub dev_engines_package_manager: Option<String>,
    pub workspaces: Option<Vec<String>>,
    /// `prisma.schema`.
    pub prisma_schema: Option<String>,
}

/// Gradle and Maven files with lowercased text for substring checks.
#[derive(Debug, Default)]
pub(crate) struct Jvm {
    docs: Vec<(String, String)>,
}

impl Jvm {
    /// Files whose text contains `needle` (lowercase).
    pub(crate) fn mentioning(&self, needle: &str) -> Vec<String> {
        self.docs
            .iter()
            .filter(|(_, text)| text.contains(needle))
            .map(|(file, _)| file.clone())
            .collect()
    }
}

/// One directory the scan treats as a project root.
#[derive(Debug)]
pub(crate) struct Project {
    /// Directory relative to the scan root; `"."` for the root.
    pub path: String,
    pub dir: Rc<DirIndex>,
    pub js: Option<JsManifest>,
    pub js_deps: Deps,
    pub py: Deps,
    pub rb: Deps,
    pub php: Deps,
    pub composer_scripts: Vec<String>,
    pub ex: Deps,
    pub go: Deps,
    pub rs: Deps,
    /// `[workspace] members` globs of a Cargo workspace root.
    pub rs_members: Vec<String>,
    pub jvm: Jvm,
}

impl Project {
    /// Path of `name` inside this project, relative to the scan root.
    pub(crate) fn file(&self, name: &str) -> String {
        join(&self.path, name)
    }

    pub(crate) fn has_file(&self, name: &str) -> bool {
        self.dir.has_file(name)
    }

    pub(crate) fn has_dir(&self, name: &str) -> bool {
        self.dir.has_dir(name)
    }
}

/// The workspace globs the root's package manager reads.
#[derive(Debug)]
pub(crate) struct JsWorkspaceDecl {
    /// Where they are declared: `pnpm-workspace.yaml` for pnpm, the
    /// `workspaces` field of `package.json` for npm, Yarn and Bun.
    pub source: WorkspaceSource,
    /// The globs as written.
    pub patterns: Vec<String>,
}

/// The directories a scan inspects.
pub(crate) struct Discovery {
    /// Root first, then workspace members, then nested directories.
    pub projects: Vec<Project>,
    /// The root's package manager, when the root is a JavaScript project.
    pub root_manager: Option<PackageManagerFact>,
    /// The root's workspace declaration, taken from the file its package
    /// manager reads and no other.
    pub js_workspace: Option<JsWorkspaceDecl>,
    /// Paths that are members of the root's JavaScript workspaces.
    pub js_members: BTreeSet<String>,
}

/// Find project directories: the root, the root's JavaScript workspace
/// members and Cargo workspace members, directories directly under the root,
/// and directories one level below `apps/`, `packages/`, `services/` and
/// similar containers. Only directories with a recognized manifest become projects.
pub(crate) fn discover(repo: &Repo) -> Discovery {
    let root_dir = repo.list(".");
    let root = build(repo, ".", Rc::clone(&root_dir));

    // The package manager decides which declaration is authoritative: pnpm
    // reads only pnpm-workspace.yaml, the others only package.json. Merging
    // both would make a directory a member of a workspace its tool never sees.
    let root_manager = (root.js.is_some() || root_dir.has_file("pnpm-workspace.yaml"))
        .then(|| js::resolve_package_manager(repo, &root, None));
    let js_workspace = root_manager
        .as_ref()
        .and_then(|manager| workspace_decl(repo, &root, &root_dir, manager.kind));
    let js_patterns: Vec<String> = js_workspace
        .as_ref()
        .map(|decl| decl.patterns.clone())
        .unwrap_or_default();
    let js_members: BTreeSet<String> = globs::expand(repo, ".", &js_patterns)
        .into_iter()
        .filter(|d| repo.list(d).has_file("package.json"))
        .collect();
    let cargo_members = globs::expand(repo, ".", &root.rs_members);

    let mut candidates: Vec<String> = Vec::new();
    candidates.extend(js_members.iter().cloned());
    candidates.extend(cargo_members);
    for name in root_dir.dirs().filter(|n| is_candidate_dir(n)) {
        candidates.push(name.to_string());
    }
    for container in CONTAINER_DIRS {
        if root_dir.has_dir(container) {
            let index = repo.list(container);
            for name in index.dirs().filter(|n| is_candidate_dir(n)) {
                candidates.push(join(container, name));
            }
        }
    }

    let mut seen: BTreeSet<String> = BTreeSet::from([".".to_string()]);
    let mut projects = vec![root];
    for path in candidates {
        if !seen.insert(path.clone()) {
            continue;
        }
        if projects.len() >= MAX_PROJECTS {
            repo.warn(
                ".",
                format!("only the first {MAX_PROJECTS} project directories were inspected"),
            );
            break;
        }
        let dir = repo.list(&path);
        if has_manifest(&dir) {
            projects.push(build(repo, &path, dir));
        }
    }
    Discovery {
        projects,
        root_manager,
        js_workspace,
        js_members,
    }
}

/// The workspace globs `manager` reads at the root, if it declares any. A
/// declaration the manager does not read is reported in a warning and
/// otherwise ignored.
fn workspace_decl(
    repo: &Repo,
    root: &Project,
    root_dir: &DirIndex,
    manager: PackageManager,
) -> Option<JsWorkspaceDecl> {
    let package_json = root
        .js
        .as_ref()
        .and_then(|m| m.workspaces.clone())
        .unwrap_or_default();
    let has_yaml = root_dir.has_file("pnpm-workspace.yaml");
    let (source, patterns) = if manager == PackageManager::Pnpm {
        if !package_json.is_empty() {
            repo.warn(
                "package.json",
                "`workspaces` is ignored: pnpm reads pnpm-workspace.yaml",
            );
        }
        (
            WorkspaceSource::PnpmWorkspaceYaml,
            pnpm_workspace_patterns(repo, root_dir),
        )
    } else {
        if has_yaml {
            repo.warn(
                "pnpm-workspace.yaml",
                "ignored: the package manager here reads `workspaces` in package.json",
            );
        }
        (WorkspaceSource::PackageJson, package_json)
    };
    (!patterns.is_empty()).then_some(JsWorkspaceDecl { source, patterns })
}

fn is_candidate_dir(name: &str) -> bool {
    !name.starts_with('.') && !SKIP_DIRS.contains(&name)
}

/// Whether the directory holds a manifest any detector reads.
fn has_manifest(dir: &DirIndex) -> bool {
    const FILES: [&str; 12] = [
        "package.json",
        "pyproject.toml",
        "manage.py",
        "Gemfile",
        "Gemfile.lock",
        "composer.json",
        "mix.exs",
        "go.mod",
        "Cargo.toml",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
    ];
    FILES.iter().any(|f| dir.has_file(f)) || dir.files().any(is_requirements_file)
}

fn is_requirements_file(name: &str) -> bool {
    name.starts_with("requirements") && name.ends_with(".txt")
}

fn build(repo: &Repo, path: &str, dir: Rc<DirIndex>) -> Project {
    let (js, js_deps) = if dir.has_file("package.json") {
        parse_package_json(repo, path).unzip()
    } else {
        (None, None)
    };
    let (php, composer_scripts) = if dir.has_file("composer.json") {
        parse_composer(repo, path)
    } else {
        (Deps::default(), Vec::new())
    };
    let (rs, rs_members) = if dir.has_file("Cargo.toml") {
        parse_cargo(repo, path)
    } else {
        (Deps::default(), Vec::new())
    };
    Project {
        path: path.to_string(),
        js,
        js_deps: js_deps.unwrap_or_default(),
        py: parse_python(repo, path, &dir),
        rb: parse_ruby(repo, path, &dir),
        php,
        composer_scripts,
        ex: parse_mix(repo, path, &dir),
        go: parse_go(repo, path, &dir),
        rs,
        rs_members,
        jvm: parse_jvm(repo, path, &dir),
        dir,
    }
}

fn pnpm_workspace_patterns(repo: &Repo, root: &DirIndex) -> Vec<String> {
    if !root.has_file("pnpm-workspace.yaml") {
        return Vec::new();
    }
    let Some(text) = repo.read_text("pnpm-workspace.yaml") else {
        return Vec::new();
    };
    super::yaml::parse(repo, "pnpm-workspace.yaml", &text)
        .map(|v| strings(v.get("packages")))
        .unwrap_or_default()
}

/// String elements of a JSON array (other elements are dropped).
fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// -- JavaScript ---------------------------------------------------------------

fn parse_package_json(repo: &Repo, dir: &str) -> Option<(JsManifest, Deps)> {
    let file = join(dir, "package.json");
    let text = repo.read_text(&file)?;
    let value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            repo.warn(&file, format!("invalid JSON: {e}"));
            return None;
        }
    };
    let object = value.as_object()?;
    let mut deps = Deps::default();
    deps.add_file(file.clone());
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(table) = object.get(section).and_then(Value::as_object) {
            for name in table.keys() {
                deps.add(name, format!("{file}:{section}.{name}"));
            }
        }
    }
    let scripts = object
        .get("scripts")
        .and_then(Value::as_object)
        .map(|s| {
            s.iter()
                .filter_map(|(k, v)| v.as_str().map(|body| (k.clone(), body.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let workspaces = match object.get("workspaces") {
        Some(Value::Array(_)) => Some(strings(object.get("workspaces"))),
        Some(Value::Object(o)) => Some(strings(o.get("packages"))),
        _ => None,
    };
    let dev_engines = object
        .get("devEngines")
        .and_then(|d| d.get("packageManager"))
        .and_then(|pm| match pm {
            Value::Array(items) => items.first(),
            other => Some(other),
        })
        .and_then(|pm| {
            let name = pm.get("name")?.as_str()?;
            Some(match pm.get("version").and_then(Value::as_str) {
                Some(version) => format!("{name}@{version}"),
                None => name.to_string(),
            })
        });
    let manifest = JsManifest {
        file,
        name: object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        scripts,
        package_manager: object
            .get("packageManager")
            .and_then(Value::as_str)
            .map(str::to_string),
        dev_engines_package_manager: dev_engines,
        workspaces,
        prisma_schema: object
            .get("prisma")
            .and_then(|p| p.get("schema"))
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    Some((manifest, deps))
}

// -- Python -------------------------------------------------------------------

/// PEP 503 normalization of a distribution name.
fn normalize_python(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '_' | '.' => '-',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// The distribution name at the start of a PEP 508 requirement.
fn requirement_name(requirement: &str) -> Option<String> {
    let name: String = requirement
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!name.is_empty()).then(|| normalize_python(&name))
}

fn parse_python(repo: &Repo, dir: &str, index: &DirIndex) -> Deps {
    let mut deps = Deps::default();
    if index.has_file("pyproject.toml") {
        let file = join(dir, "pyproject.toml");
        deps.add_file(file.clone());
        if let Some(table) = repo
            .read_text(&file)
            .and_then(|text| parse_toml(repo, &file, &text))
        {
            collect_pyproject(&table, &file, &mut deps);
        }
    }
    let mut requirement_files: Vec<String> = index
        .files()
        .filter(|n| is_requirements_file(n))
        .map(|n| join(dir, n))
        .collect();
    if index.has_dir("requirements") {
        let nested = repo.list(&join(dir, "requirements"));
        requirement_files.extend(
            nested
                .files()
                .filter(|n| n.ends_with(".txt"))
                .map(|n| join(&join(dir, "requirements"), n)),
        );
    }
    requirement_files.truncate(12);
    for file in requirement_files {
        deps.add_file(file.clone());
        if let Some(text) = repo.read_text(&file) {
            for line in text.lines() {
                if let Some(name) = requirements_line_name(line) {
                    deps.add(&name, format!("{file}:{name}"));
                }
            }
        }
    }
    deps
}

fn requirements_line_name(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty()
        || line.starts_with('#')
        || line.starts_with('-')
        || line.contains("://")
        || line.starts_with('.')
        || line.starts_with('/')
    {
        return None;
    }
    requirement_name(line)
}

fn collect_pyproject(table: &toml::Table, file: &str, deps: &mut Deps) {
    let add_requirements = |values: &[toml::Value], deps: &mut Deps| {
        for value in values {
            if let Some(name) = value.as_str().and_then(requirement_name) {
                deps.add(&name, format!("{file}:{name}"));
            }
        }
    };
    if let Some(project) = table.get("project").and_then(toml::Value::as_table) {
        if let Some(list) = project.get("dependencies").and_then(toml::Value::as_array) {
            add_requirements(list, deps);
        }
        if let Some(optional) = project
            .get("optional-dependencies")
            .and_then(toml::Value::as_table)
        {
            for list in optional.values().filter_map(toml::Value::as_array) {
                add_requirements(list, deps);
            }
        }
    }
    if let Some(groups) = table
        .get("dependency-groups")
        .and_then(toml::Value::as_table)
    {
        for list in groups.values().filter_map(toml::Value::as_array) {
            add_requirements(list, deps);
        }
    }
    let tool = table.get("tool").and_then(toml::Value::as_table);
    if tool.is_some_and(|t| t.contains_key("uv")) {
        deps.markers.push("tool.uv");
    }
    if tool.is_some_and(|t| t.contains_key("poetry")) {
        deps.markers.push("tool.poetry");
    }
    if let Some(list) = tool
        .and_then(|t| t.get("uv"))
        .and_then(|u| u.get("dev-dependencies"))
        .and_then(toml::Value::as_array)
    {
        add_requirements(list, deps);
    }
    if let Some(poetry) = tool
        .and_then(|t| t.get("poetry"))
        .and_then(toml::Value::as_table)
    {
        let mut tables: Vec<&toml::Table> = Vec::new();
        for key in ["dependencies", "dev-dependencies"] {
            tables.extend(poetry.get(key).and_then(toml::Value::as_table));
        }
        if let Some(groups) = poetry.get("group").and_then(toml::Value::as_table) {
            for group in groups.values().filter_map(toml::Value::as_table) {
                tables.extend(group.get("dependencies").and_then(toml::Value::as_table));
            }
        }
        for table in tables {
            for name in table.keys().filter(|n| n.as_str() != "python") {
                let name = normalize_python(name);
                deps.add(&name, format!("{file}:{name}"));
            }
        }
    }
}

// -- Ruby ---------------------------------------------------------------------

fn parse_ruby(repo: &Repo, dir: &str, index: &DirIndex) -> Deps {
    let mut deps = Deps::default();
    if index.has_file("Gemfile.lock") {
        let file = join(dir, "Gemfile.lock");
        deps.add_file(file.clone());
        if let Some(text) = repo.read_text(&file) {
            let mut section = "";
            for line in text.lines() {
                if !line.starts_with(' ') && !line.trim().is_empty() {
                    section = line.trim();
                    continue;
                }
                let gem = match section {
                    // `    name (1.2.3)` under `specs:`; deeper lines are constraints.
                    "GEM" | "GIT" | "PATH"
                        if line.starts_with("    ") && !line.starts_with("     ") =>
                    {
                        gem_name(line)
                    }
                    "DEPENDENCIES" if line.starts_with("  ") && !line.starts_with("   ") => {
                        gem_name(line)
                    }
                    _ => None,
                };
                if let Some(gem) = gem {
                    deps.add(&gem, format!("{file}:{gem}"));
                }
            }
        }
    }
    if index.has_file("Gemfile") {
        let file = join(dir, "Gemfile");
        deps.add_file(file.clone());
        if !index.has_file("Gemfile.lock")
            && let Some(text) = repo.read_text(&file)
        {
            for line in text.lines() {
                let Some(rest) = line.trim().strip_prefix("gem ") else {
                    continue;
                };
                let rest = rest.trim_start();
                if let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\''))
                    && let Some(end) = rest[1..].find(quote)
                {
                    let gem = &rest[1..1 + end];
                    deps.add(gem, format!("{file}:{gem}"));
                }
            }
        }
    }
    deps
}

fn gem_name(line: &str) -> Option<String> {
    let name: String = line
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!name.is_empty()).then_some(name)
}

// -- PHP ----------------------------------------------------------------------

fn parse_composer(repo: &Repo, dir: &str) -> (Deps, Vec<String>) {
    let file = join(dir, "composer.json");
    let mut deps = Deps::default();
    deps.add_file(file.clone());
    let mut scripts = Vec::new();
    if let Some(text) = repo.read_text(&file) {
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => {
                for section in ["require", "require-dev"] {
                    if let Some(table) = value.get(section).and_then(Value::as_object) {
                        for name in table.keys() {
                            deps.add(name, format!("{file}:{section}.{name}"));
                        }
                    }
                }
                if let Some(table) = value.get("scripts").and_then(Value::as_object) {
                    scripts = table.keys().cloned().collect();
                }
            }
            Err(e) => repo.warn(&file, format!("invalid JSON: {e}")),
        }
    }
    (deps, scripts)
}

// -- Elixir -------------------------------------------------------------------

fn parse_mix(repo: &Repo, dir: &str, index: &DirIndex) -> Deps {
    let mut deps = Deps::default();
    if !index.has_file("mix.exs") {
        return deps;
    }
    let file = join(dir, "mix.exs");
    deps.add_file(file.clone());
    if let Some(text) = repo.read_text(&file) {
        // Dependency tuples look like `{:phoenix, "~> 1.7"}`.
        let mut rest = text.as_str();
        while let Some(at) = rest.find("{:") {
            let after = &rest[at + 2..];
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() && after[name.len()..].trim_start().starts_with([',', '}']) {
                deps.add(&name, format!("{file}:{name}"));
            }
            rest = after;
        }
    }
    deps
}

// -- Go -----------------------------------------------------------------------

fn parse_go(repo: &Repo, dir: &str, index: &DirIndex) -> Deps {
    let mut deps = Deps::default();
    if !index.has_file("go.mod") {
        return deps;
    }
    let file = join(dir, "go.mod");
    deps.add_file(file.clone());
    if let Some(text) = repo.read_text(&file) {
        let mut in_block = false;
        for line in text.lines() {
            let line = line.split("//").next().unwrap_or("").trim();
            if in_block {
                if line.starts_with(')') {
                    in_block = false;
                } else if let Some(module) = line.split_whitespace().next() {
                    deps.add(module, format!("{file}:{module}"));
                }
            } else if let Some(rest) = line.strip_prefix("require") {
                let rest = rest.trim();
                if rest.starts_with('(') {
                    in_block = true;
                } else if let Some(module) = rest.split_whitespace().next() {
                    deps.add(module, format!("{file}:{module}"));
                }
            }
        }
    }
    deps
}

// -- Rust ---------------------------------------------------------------------

fn parse_cargo(repo: &Repo, dir: &str) -> (Deps, Vec<String>) {
    let file = join(dir, "Cargo.toml");
    let mut deps = Deps::default();
    deps.add_file(file.clone());
    let mut members = Vec::new();
    let Some(table) = repo
        .read_text(&file)
        .and_then(|text| parse_toml(repo, &file, &text))
    else {
        return (deps, members);
    };
    let mut sections: Vec<(String, &toml::Table)> = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(t) = table.get(key).and_then(toml::Value::as_table) {
            sections.push((key.to_string(), t));
        }
    }
    if let Some(workspace) = table.get("workspace").and_then(toml::Value::as_table) {
        if let Some(t) = workspace
            .get("dependencies")
            .and_then(toml::Value::as_table)
        {
            sections.push(("workspace.dependencies".to_string(), t));
        }
        members = workspace
            .get("members")
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
    }
    for (section, entries) in sections {
        for (key, value) in entries {
            // `foo = { package = "real-name", ... }` renames the crate.
            let name = value
                .get("package")
                .and_then(toml::Value::as_str)
                .unwrap_or(key);
            deps.add(name, format!("{file}:{section}.{key}"));
            if let Some(features) = value.get("features").and_then(toml::Value::as_array) {
                deps.features.entry(name.to_string()).or_default().extend(
                    features
                        .iter()
                        .filter_map(|f| f.as_str().map(str::to_string)),
                );
            }
        }
    }
    (deps, members)
}

// -- JVM ----------------------------------------------------------------------

fn parse_jvm(repo: &Repo, dir: &str, index: &DirIndex) -> Jvm {
    let mut jvm = Jvm::default();
    for name in [
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        "gradle.properties",
    ] {
        if !index.has_file(name) {
            continue;
        }
        let file = join(dir, name);
        if let Some(text) = repo.read_text(&file) {
            jvm.docs.push((file, text.to_ascii_lowercase()));
        }
    }
    jvm
}

pub(crate) fn parse_toml(repo: &Repo, file: &str, text: &str) -> Option<toml::Table> {
    match text.parse::<toml::Table>() {
        Ok(table) => Some(table),
        Err(e) => {
            let message = e.message().to_string();
            repo.warn(file, format!("invalid TOML: {message}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirement_names_are_normalized() {
        assert_eq!(requirement_name("Django>=4.2"), Some("django".into()));
        assert_eq!(
            requirement_name("psycopg[binary]==3.1"),
            Some("psycopg".into())
        );
        assert_eq!(
            requirement_name("typing_extensions ; python_version<'3.9'"),
            Some("typing-extensions".into())
        );
        assert_eq!(requirements_line_name("-r base.txt"), None);
        assert_eq!(requirements_line_name("git+https://x/y.git#egg=z"), None);
        assert_eq!(requirements_line_name("# comment"), None);
    }

    #[test]
    fn gem_names_stop_at_the_version() {
        assert_eq!(gem_name("    rails (7.1.3)"), Some("rails".into()));
        assert_eq!(gem_name("  pg (~> 1.1)"), Some("pg".into()));
    }
}
