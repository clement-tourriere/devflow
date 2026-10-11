//! JavaScript package managers, workspaces and packages.
use super::facts::{
    JsPackage, JsProject, MonorepoTool, MonorepoToolFact, PackageManager, PackageManagerFact,
    PackageManagerSource, WorkspaceFacts, WorkspaceSource,
};
use super::manifest::{Discovery, JsManifest, Project};
use super::push_unique;
use super::repo::Repo;

/// Lockfiles in priority order when several exist.
const LOCKFILES: [(&str, PackageManager); 6] = [
    ("bun.lock", PackageManager::Bun),
    ("bun.lockb", PackageManager::Bun),
    ("pnpm-lock.yaml", PackageManager::Pnpm),
    ("yarn.lock", PackageManager::Yarn),
    ("package-lock.json", PackageManager::Npm),
    ("npm-shrinkwrap.json", PackageManager::Npm),
];

pub(super) fn detect(repo: &Repo, discovery: &Discovery) -> Vec<JsProject> {
    let mut out: Vec<JsProject> = Vec::new();
    let Some(root) = discovery.projects.first() else {
        return out;
    };
    if let Some(manager) = &discovery.root_manager {
        out.push(root_project(discovery, root, manager.clone()));
    }
    let inherit = out.first().map(|p| p.package_manager.clone());
    for project in discovery.projects.iter().skip(1) {
        if project.js.is_none() || discovery.js_members.contains(&project.path) {
            continue;
        }
        let package_manager = resolve_package_manager(repo, project, inherit.as_ref());
        out.push(JsProject {
            path: project.path.clone(),
            package_manager,
            workspaces: None,
            monorepo_tools: monorepo_tools(project),
            packages: package_of(project, false).into_iter().collect(),
            evidence: project.js.iter().map(|m| m.file.clone()).collect(),
        });
    }
    out
}

fn root_project(
    discovery: &Discovery,
    root: &Project,
    package_manager: PackageManagerFact,
) -> JsProject {
    let mut packages: Vec<JsPackage> = package_of(root, false).into_iter().collect();
    for member in &discovery.js_members {
        if let Some(project) = discovery.projects.iter().find(|p| &p.path == member)
            && let Some(package) = package_of(project, true)
        {
            packages.push(package);
        }
    }
    let workspaces = discovery.js_workspace.as_ref().map(|decl| WorkspaceFacts {
        source: decl.source,
        patterns: decl.patterns.clone(),
        evidence: vec![match decl.source {
            WorkspaceSource::PackageJson => "package.json:workspaces".to_string(),
            WorkspaceSource::PnpmWorkspaceYaml => "pnpm-workspace.yaml:packages".to_string(),
        }],
    });
    let mut evidence: Vec<String> = root.js.iter().map(|m| m.file.clone()).collect();
    if let Some(w) = &workspaces {
        for e in &w.evidence {
            push_unique(&mut evidence, e.clone());
        }
    }
    JsProject {
        path: ".".to_string(),
        package_manager,
        workspaces,
        monorepo_tools: monorepo_tools(root),
        packages,
        evidence,
    }
}

fn package_of(project: &Project, workspace_member: bool) -> Option<JsPackage> {
    let manifest = project.js.as_ref()?;
    let scripts: Vec<String> = manifest.scripts.keys().cloned().collect();
    let prisma_generate_scripts = manifest
        .scripts
        .iter()
        .filter(|(_, body)| body.contains("prisma generate"))
        .map(|(name, _)| name.clone())
        .collect();
    Some(JsPackage {
        path: project.path.clone(),
        name: manifest.name.clone(),
        scripts,
        prisma_generate_scripts,
        workspace_member,
        evidence: vec![manifest.file.clone()],
    })
}

fn monorepo_tools(project: &Project) -> Vec<MonorepoToolFact> {
    let mut out = Vec::new();
    let tools: [(MonorepoTool, &[&str], &[&str]); 3] = [
        (
            MonorepoTool::Turbo,
            &["turbo.json", "turbo.jsonc"],
            &["turbo"],
        ),
        (MonorepoTool::Nx, &["nx.json"], &["nx", "@nrwl/workspace"]),
        (MonorepoTool::Lerna, &["lerna.json"], &["lerna"]),
    ];
    for (tool, files, deps) in tools {
        let mut evidence = Vec::new();
        for file in files {
            if project.has_file(file) {
                evidence.push(project.file(file));
            }
        }
        for dep in deps {
            if let Some(e) = project.js_deps.evidence(dep) {
                evidence.push(e);
            }
        }
        if !evidence.is_empty() {
            out.push(MonorepoToolFact { tool, evidence });
        }
    }
    out
}

/// The package manager that installs `project`, with its own lockfile (a
/// lockfile of another tool, or the inherited root's, does not count).
pub(super) fn resolve_package_manager(
    repo: &Repo,
    project: &Project,
    inherit: Option<&PackageManagerFact>,
) -> PackageManagerFact {
    let mut manager = choose_package_manager(repo, project, inherit);
    manager.lockfile = LOCKFILES
        .iter()
        .find(|(file, kind)| *kind == manager.kind && project.has_file(file))
        .map(|(file, _)| project.file(file));
    manager
}

fn choose_package_manager(
    repo: &Repo,
    project: &Project,
    inherit: Option<&PackageManagerFact>,
) -> PackageManagerFact {
    let berry_files = project.has_file(".yarnrc.yml");
    let manifest: Option<&JsManifest> = project.js.as_ref();

    // 1. Explicit pins win over anything on disk.
    if let Some(m) = manifest {
        for (value, key, source) in [
            (
                m.package_manager.as_deref(),
                "packageManager",
                PackageManagerSource::PackageManagerField,
            ),
            (
                m.dev_engines_package_manager.as_deref(),
                "devEngines.packageManager",
                PackageManagerSource::DevEngines,
            ),
        ] {
            if let Some((kind, version)) = value.and_then(parse_pin) {
                let major = version.as_deref().and_then(major_version);
                let yarn_berry = kind == PackageManager::Yarn
                    && (major.is_some_and(|m| m >= 2) || (major.is_none() && berry_files));
                return PackageManagerFact {
                    kind,
                    version,
                    yarn_berry,
                    source,
                    lockfile: None,
                    evidence: vec![format!("{}:{key}", m.file)],
                };
            }
        }
    }

    // 2. Lockfiles.
    let present: Vec<(&str, PackageManager)> = LOCKFILES
        .iter()
        .filter(|(file, _)| project.has_file(file))
        .copied()
        .collect();
    if let Some(&(file, kind)) = present.first() {
        let lock = project.file(file);
        let others: Vec<&str> = present
            .iter()
            .filter(|(_, k)| *k != kind)
            .map(|(f, _)| *f)
            .collect();
        if !others.is_empty() {
            repo.warn(
                &project.path,
                format!(
                    "several lockfiles ({}, {}); using {file}",
                    file,
                    others.join(", ")
                ),
            );
        }
        let mut evidence = vec![lock.clone()];
        let yarn_berry = kind == PackageManager::Yarn && {
            if berry_files {
                evidence.push(project.file(".yarnrc.yml"));
                true
            } else {
                repo.read_head(&lock, 2048)
                    .is_some_and(|head| head.contains("__metadata:"))
            }
        };
        return PackageManagerFact {
            kind,
            version: None,
            yarn_berry,
            source: PackageManagerSource::Lockfile,
            lockfile: None,
            evidence,
        };
    }

    // 3. Tool-specific configuration without a lockfile (nothing installed yet).
    for (file, kind) in [
        ("pnpm-workspace.yaml", PackageManager::Pnpm),
        (".yarnrc.yml", PackageManager::Yarn),
        ("bunfig.toml", PackageManager::Bun),
    ] {
        if project.has_file(file) {
            return PackageManagerFact {
                kind,
                version: None,
                yarn_berry: kind == PackageManager::Yarn,
                source: PackageManagerSource::ConfigFile,
                lockfile: None,
                evidence: vec![project.file(file)],
            };
        }
    }

    // 4. Inherit from the repository root, else guess npm.
    if let Some(parent) = inherit {
        return PackageManagerFact {
            source: PackageManagerSource::Inherited,
            ..parent.clone()
        };
    }
    PackageManagerFact {
        kind: PackageManager::Npm,
        version: None,
        yarn_berry: false,
        source: PackageManagerSource::Assumed,
        lockfile: None,
        evidence: manifest.iter().map(|m| m.file.clone()).collect(),
    }
}

/// `pnpm@9.1.0+sha512.abc` becomes `(Pnpm, Some("9.1.0"))`; `bun` becomes `(Bun, None)`.
fn parse_pin(value: &str) -> Option<(PackageManager, Option<String>)> {
    let (name, rest) = match value.split_once('@') {
        Some((name, rest)) => (name, Some(rest)),
        None => (value, None),
    };
    let kind = match name.trim() {
        "npm" => PackageManager::Npm,
        "pnpm" => PackageManager::Pnpm,
        "yarn" => PackageManager::Yarn,
        "bun" => PackageManager::Bun,
        _ => return None,
    };
    let version = rest
        .map(|r| r.split('+').next().unwrap_or(r).trim().to_string())
        .filter(|v| !v.is_empty());
    Some((kind, version))
}

fn major_version(version: &str) -> Option<u32> {
    version
        .trim_start_matches(['^', '~', 'v', '=', '>', '<', ' '])
        .split('.')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_manager_pins() {
        assert_eq!(
            parse_pin("pnpm@9.1.0+sha512.abc"),
            Some((PackageManager::Pnpm, Some("9.1.0".into())))
        );
        assert_eq!(parse_pin("bun"), Some((PackageManager::Bun, None)));
        assert_eq!(parse_pin("deno@2.0.0"), None);
        assert_eq!(major_version("^4.1.0"), Some(4));
        assert_eq!(major_version("latest"), None);
    }
}
