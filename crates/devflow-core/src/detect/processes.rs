//! Processes a developer runs while working on the project.
//!
//! A process is reported as an identity (`name`, `cwd`, `source`, `role`), a
//! way to start it ([`ProcessLaunch`]: an argument vector built by the scan,
//! or a pointer to a Procfile line) and a `display` string for people.
//!
//! No command text that comes from the repository is retained. Script bodies
//! are not (package scripts and mise tasks are referenced by name, as one
//! argument), and neither is a Procfile command line, which often inlines
//! credentials in forms no masking recognizes (`--password hunter2`): a
//! Procfile process is displayed as `<file>: <process>` and carries a
//! pointer to its line.
use super::facts::{
    Framework, FrameworkFact, JsProject, PackageManagerFact, ProcessFact, ProcessLaunch,
    ProcessRole, ProcessSource, PythonProject, PythonTool, ToolchainFacts, WiringFacts,
};
use super::manifest::{Discovery, Project};
use super::repo::{Repo, join};
use crate::redact;
use std::collections::BTreeMap;

/// Procfile names, in the order they are read.
const PROCFILES: [&str; 2] = ["Procfile", "Procfile.dev"];
const MAX_PROCESSES: usize = 80;
const MAX_DISPLAY: usize = 300;
/// Longest script or task name that becomes a process.
const MAX_NAME: usize = 128;

pub(super) fn detect(
    repo: &Repo,
    discovery: &Discovery,
    js: &[JsProject],
    python: &[PythonProject],
    frameworks: &[FrameworkFact],
    toolchain: &ToolchainFacts,
    wiring: &WiringFacts,
) -> Vec<ProcessFact> {
    // Every source adds to one collection that refuses to grow past the cap,
    // and a source is not read once the cap is reached: the limit applies
    // while processes are collected, not after a hostile Procfile of a
    // million lines has been turned into a million facts.
    let mut out = Processes::default();
    for project in &discovery.projects {
        if !out.has_room() {
            break;
        }
        procfiles(repo, project, &mut out);
    }
    for framework in frameworks {
        if !out.has_room() {
            break;
        }
        let Some(project) = discovery.projects.iter().find(|p| p.path == framework.path) else {
            continue;
        };
        match framework.framework {
            Framework::Rails => rails(repo, project, &mut out),
            Framework::Django => django(project, python, &mut out),
            Framework::Phoenix => phoenix(project, &mut out),
            Framework::Laravel => composer(project, &mut out),
            Framework::SpringBoot => spring(project, &mut out),
            _ => {}
        }
    }
    if out.has_room() {
        package_scripts(repo, js, &mut out);
    }
    if out.has_room() {
        mise_tasks(repo, toolchain, &mut out);
    }
    if let Some(pitchfork) = &wiring.pitchfork {
        for daemon in &pitchfork.daemons {
            let added = out.push_with(|| ProcessFact {
                name: daemon.name.clone(),
                display: None,
                launch: None,
                cwd: ".".into(),
                source: ProcessSource::Pitchfork,
                role: role_of(&daemon.name, ProcessRole::Other),
                evidence: vec![format!("{}:daemons.{}", pitchfork.file, daemon.name)],
            });
            if !added {
                break;
            }
        }
    }
    if out.truncated {
        repo.warn(
            ".",
            format!(
                "the limit of {MAX_PROCESSES} processes was reached; further processes are not reported"
            ),
        );
    }
    out.list
}

/// The processes found so far, never more than [`MAX_PROCESSES`].
#[derive(Default)]
struct Processes {
    list: Vec<ProcessFact>,
    /// A process was dropped, or a source skipped, because the list was full.
    truncated: bool,
}

impl Processes {
    /// Whether another process fits. A full list is marked truncated: whatever
    /// the caller was about to look at is not looked at.
    fn has_room(&mut self) -> bool {
        if self.list.len() >= MAX_PROCESSES {
            self.truncated = true;
            return false;
        }
        true
    }

    /// Add the process `make` builds, if there is room; when there is none
    /// `make` does not run, so a full list costs no allocation. `false` when
    /// the list is full, which tells the caller to stop.
    fn push_with(&mut self, make: impl FnOnce() -> ProcessFact) -> bool {
        if !self.has_room() {
            return false;
        }
        self.list.push(make());
        true
    }

    /// Add a process that is already built (a source that yields a handful),
    /// unless the list is full.
    fn push(&mut self, process: ProcessFact) {
        self.push_with(|| process);
    }
}

/// Role from a conventional name, falling back to `default`.
fn role_of(name: &str, default: ProcessRole) -> ProcessRole {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "web" | "start" | "serve" | "server" => ProcessRole::Serve,
        "release" | "migrate" => ProcessRole::Release,
        "dev" | "watch" | "js" | "css" | "assets" | "vite" | "webpack" | "esbuild" | "tailwind" => {
            ProcessRole::Dev
        }
        _ if lower.starts_with("dev:") || lower.starts_with("dev-") => ProcessRole::Dev,
        _ if lower.contains("worker")
            || lower.contains("sidekiq")
            || lower.contains("queue")
            || lower.contains("jobs") =>
        {
            ProcessRole::Worker
        }
        _ => default,
    }
}

/// Text for people: control characters escaped (a name must not move the
/// cursor or recolor a terminal), credentials masked, then clipped. Masking
/// runs on the whole text first, so clipping can never cut a credential in
/// half and leave a recognizable prefix.
fn displayable(text: &str) -> String {
    let escaped: String = text
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect();
    redact::text(&escaped).chars().take(MAX_DISPLAY).collect()
}

/// An argument-vector process: `display` is the shell rendering of `argv`.
fn argv_process(
    argv: Vec<String>,
    name: &str,
    cwd: &str,
    source: ProcessSource,
    role: ProcessRole,
    evidence: Vec<String>,
) -> ProcessFact {
    let launch = ProcessLaunch::Argv { argv };
    ProcessFact {
        name: name.to_string(),
        display: launch.shell_line().map(|line| displayable(&line)),
        launch: Some(launch),
        cwd: cwd.to_string(),
        source,
        role,
        evidence,
    }
}

fn words(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|w| (*w).to_string()).collect()
}

/// Whether a script or task name may become an argument of a command: no
/// control characters, not an option (`--prefix=/x`), of sensible length.
fn is_launchable_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME
        && !name.starts_with('-')
        && !name.chars().any(char::is_control)
}

fn procfiles(repo: &Repo, project: &Project, out: &mut Processes) {
    for name in PROCFILES {
        if !project.has_file(name) {
            continue;
        }
        if !out.has_room() {
            return;
        }
        let file = project.file(name);
        let Some(text) = repo.read_text(&file) else {
            continue;
        };
        let default_role = if name == "Procfile.dev" {
            ProcessRole::Dev
        } else {
            ProcessRole::Other
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((process, _)) = line.split_once(':') else {
                continue;
            };
            let process = process.trim();
            // A name is copied into several fields, so its length is bounded
            // like the name of a script.
            let valid = !process.is_empty()
                && process.len() <= MAX_NAME
                && process
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
            if !valid {
                continue;
            }
            // Past the cap nothing is built: the rest of the file is not read.
            let added = out.push_with(|| ProcessFact {
                name: process.to_string(),
                // The command line is never copied into the facts, masked or
                // not: masking recognizes `--password=x` and `password: x`, not
                // `--password x`, so what is left would still carry secrets.
                // The line is found again through `launch`.
                display: Some(displayable(&format!("{file}: {process}"))),
                launch: Some(ProcessLaunch::Procfile {
                    file: file.clone(),
                    process: process.to_string(),
                }),
                cwd: project.path.clone(),
                source: ProcessSource::Procfile,
                role: role_of(process, default_role),
                evidence: vec![format!("{file}:{process}")],
            });
            if !added {
                return;
            }
        }
    }
}

fn rails(repo: &Repo, project: &Project, out: &mut Processes) {
    if project.has_dir("bin") && repo.is_file(&project.file("bin/dev")) {
        out.push(argv_process(
            words(&["bin/dev"]),
            "dev",
            &project.path,
            ProcessSource::RailsBinDev,
            ProcessRole::Dev,
            vec![project.file("bin/dev")],
        ));
    }
}

fn django(project: &Project, python: &[PythonProject], out: &mut Processes) {
    let runner = python
        .iter()
        .filter(|p| {
            p.path == "."
                || project.path == p.path
                || project.path.starts_with(&format!("{}/", p.path))
        })
        .map(|p| p.tool)
        .next();
    let mut argv = match runner {
        Some(PythonTool::Uv) => words(&["uv", "run"]),
        Some(PythonTool::Poetry) => words(&["poetry", "run"]),
        _ => Vec::new(),
    };
    argv.extend(words(&["python", "manage.py", "runserver"]));
    out.push(argv_process(
        argv,
        "runserver",
        &project.path,
        ProcessSource::DjangoRunserver,
        ProcessRole::Dev,
        vec![project.file("manage.py")],
    ));
}

fn phoenix(project: &Project, out: &mut Processes) {
    out.push(argv_process(
        words(&["mix", "phx.server"]),
        "phx.server",
        &project.path,
        ProcessSource::Phoenix,
        ProcessRole::Dev,
        project.ex.evidence("phoenix").into_iter().collect(),
    ));
}

/// Laravel 11+ ships a `dev` Composer script that starts server, queue and Vite.
fn composer(project: &Project, out: &mut Processes) {
    for script in ["dev", "serve"] {
        if project.composer_scripts.iter().any(|s| s == script) {
            out.push(argv_process(
                words(&["composer", "run", script]),
                script,
                &project.path,
                ProcessSource::Composer,
                role_of(script, ProcessRole::Dev),
                vec![format!(
                    "{}:scripts.{script}",
                    project.file("composer.json")
                )],
            ));
        }
    }
}

fn spring(project: &Project, out: &mut Processes) {
    let (argv, evidence) = if project.has_file("pom.xml") {
        let wrapper = project.has_file("mvnw");
        (
            if wrapper {
                words(&["./mvnw", "spring-boot:run"])
            } else {
                words(&["mvn", "spring-boot:run"])
            },
            project.file("pom.xml"),
        )
    } else {
        let wrapper = project.has_file("gradlew");
        let build = if project.has_file("build.gradle.kts") {
            "build.gradle.kts"
        } else {
            "build.gradle"
        };
        (
            if wrapper {
                words(&["./gradlew", "bootRun"])
            } else {
                words(&["gradle", "bootRun"])
            },
            project.file(build),
        )
    };
    out.push(argv_process(
        argv,
        "bootRun",
        &project.path,
        ProcessSource::SpringBoot,
        ProcessRole::Dev,
        vec![evidence],
    ));
}

/// `dev`, `start`, `serve` and `worker` scripts, with their conventional variants.
fn script_role(name: &str) -> Option<ProcessRole> {
    match name {
        "dev" | "start:dev" | "serve:dev" => Some(ProcessRole::Dev),
        "start" | "serve" => Some(ProcessRole::Serve),
        "worker" | "start:worker" => Some(ProcessRole::Worker),
        _ if name.starts_with("dev:") => Some(ProcessRole::Dev),
        _ if name.starts_with("worker:") => Some(ProcessRole::Worker),
        _ => None,
    }
}

fn package_scripts(repo: &Repo, js: &[JsProject], out: &mut Processes) {
    let managers: BTreeMap<&str, &PackageManagerFact> = js
        .iter()
        .flat_map(|project| {
            project
                .packages
                .iter()
                .map(move |package| (package.path.as_str(), &project.package_manager))
        })
        .collect();
    for project in js {
        for package in &project.packages {
            let Some(manager) = managers.get(package.path.as_str()) else {
                continue;
            };
            for script in &package.scripts {
                let Some(role) = script_role(script) else {
                    continue;
                };
                if !is_launchable_name(script) {
                    repo.warn(
                        &join(&package.path, "package.json"),
                        format!(
                            "script '{}' has an unusable name; not reported as a process",
                            displayable(script)
                        ),
                    );
                    continue;
                }
                let added = out.push_with(|| {
                    let mut argv: Vec<String> = manager
                        .run_argv()
                        .iter()
                        .map(|w| (*w).to_string())
                        .collect();
                    // The script name is one argument, whatever characters it has.
                    argv.push(script.clone());
                    argv_process(
                        argv,
                        script,
                        &package.path,
                        ProcessSource::PackageJson,
                        role,
                        vec![format!(
                            "{}:scripts.{script}",
                            join(&package.path, "package.json")
                        )],
                    )
                });
                if !added {
                    return;
                }
            }
        }
    }
}

fn mise_tasks(repo: &Repo, toolchain: &ToolchainFacts, out: &mut Processes) {
    let Some(mise) = &toolchain.mise else {
        return;
    };
    for task in &mise.tasks {
        let role = match task.as_str() {
            "dev" | "watch" => ProcessRole::Dev,
            "start" | "serve" => ProcessRole::Serve,
            "worker" => ProcessRole::Worker,
            _ if task.starts_with("dev:") => ProcessRole::Dev,
            _ => continue,
        };
        if !is_launchable_name(task) {
            repo.warn(
                ".",
                format!(
                    "mise task '{}' has an unusable name; not reported as a process",
                    displayable(task)
                ),
            );
            continue;
        }
        let added = out.push_with(|| {
            let evidence = mise
                .evidence
                .iter()
                .filter(|e| e.ends_with(&format!(":tasks.{task}")))
                .cloned()
                .chain(mise.config_files.first().cloned())
                .take(1)
                .collect();
            argv_process(
                vec!["mise".into(), "run".into(), task.clone()],
                task,
                ".",
                ProcessSource::MiseTask,
                role,
                evidence,
            )
        });
        if !added {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::facts::shell_quote;
    use super::*;

    #[test]
    fn displays_are_masked_escaped_and_clipped() {
        // kingfisher:ignore
        let shown = displayable("env DATABASE_URL=postgresql://alice:SECRET@host/db python app.py");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("alice"), "{shown}");
        assert_eq!(displayable("a\u{1b}[2Jb"), "a\\u{1b}[2Jb");
        assert_eq!(displayable(&"x".repeat(1000)).chars().count(), MAX_DISPLAY);
        // A credential straddling the clip point is masked before clipping.
        let long = format!("{} PGPASSWORD=hunter2hunter2", "x".repeat(MAX_DISPLAY - 8));
        assert!(!displayable(&long).contains("hunter2"));
    }

    fn process_named(name: &str) -> ProcessFact {
        argv_process(
            words(&["x"]),
            name,
            ".",
            ProcessSource::Phoenix,
            ProcessRole::Dev,
            Vec::new(),
        )
    }

    /// Review round 3, finding 4: a full list builds nothing more, so a source
    /// with a million entries costs 80 facts, not a million.
    #[test]
    fn r3_finding_4_a_full_list_builds_no_more_facts() {
        let built = std::cell::Cell::new(0);
        let mut out = Processes::default();
        let mut offered = 0;
        for i in 0..1_000_000 {
            offered += 1;
            let added = out.push_with(|| {
                built.set(built.get() + 1);
                process_named(&i.to_string())
            });
            if !added {
                break;
            }
        }
        assert_eq!(built.get(), MAX_PROCESSES);
        assert_eq!(offered, MAX_PROCESSES + 1, "the caller is told to stop");
        assert!(out.truncated);
        assert!(!out.push_with(|| unreachable!("a full list builds nothing")));
        out.push(process_named("late"));
        assert_eq!(out.list.len(), MAX_PROCESSES);
        assert_eq!(out.list[0].name, "0");
    }

    #[test]
    fn r3_finding_4_a_list_that_just_fits_is_not_truncated() {
        let mut out = Processes::default();
        for i in 0..MAX_PROCESSES {
            assert!(out.push_with(|| process_named(&i.to_string())));
        }
        assert!(!out.truncated);
        assert!(!out.has_room());
        assert!(out.truncated, "asking for one more is what marks it");
    }

    #[test]
    fn names_with_shell_syntax_stay_one_argument() {
        let launch = ProcessLaunch::Argv {
            argv: vec![
                "npm".into(),
                "run".into(),
                "dev:web app; touch pwned".into(),
            ],
        };
        assert_eq!(
            launch.shell_line().as_deref(),
            Some("npm run 'dev:web app; touch pwned'")
        );
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("dev:web"), "dev:web");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(id)"), "'$(id)'");
        assert!(!is_launchable_name("--prefix=/x"));
        assert!(!is_launchable_name("dev:\u{1b}[31m"));
        assert!(is_launchable_name("dev:web app"));
    }
}
