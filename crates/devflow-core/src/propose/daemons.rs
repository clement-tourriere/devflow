//! pitchfork daemons for the processes a developer runs.
//!
//! Commands come from [`ProcessLaunch`] argument vectors (script and task
//! names are single quoted words), never from command text in the repository.
//! A Procfile entry's command line is not in the facts, so a Procfile process
//! is only a note asking the person to copy the command.
use super::existing::PITCHFORK_FILE;
use super::hooks::slug;
use super::plan::{Builder, Pushed, Spec};
use super::{Confidence, ItemKind, PatchOp};
use crate::detect::{
    Framework, FrameworkFact, PackageManager, PackageManagerFact, ProcessFact, ProcessLaunch,
    ProcessRole, ProcessSource,
};
use serde_json::json;
use std::collections::BTreeSet;

/// Most daemons one plan proposes.
const MAX_DAEMONS: usize = 8;

struct Candidate {
    name: String,
    /// The command without port arguments, to recognize an existing daemon.
    base: String,
    run: String,
    dir: Option<String>,
    port: Option<u16>,
    confidence: Confidence,
    evidence: Vec<String>,
    risks: Vec<String>,
    /// Lower wins when two processes want the same daemon name.
    rank: u8,
}

/// Daemons that already exist in pitchfork's files, by name, with their commands.
struct ExistingDaemons {
    names: BTreeSet<String>,
    runs: Vec<String>,
}

impl Builder<'_> {
    pub fn daemons(&mut self) {
        let facts = self.facts;
        let candidates = self.daemon_candidates();
        if candidates.is_empty() {
            return;
        }
        let existing = self.existing_daemons();
        // `mise = true` is for the tools mise pins. The workspace's variables
        // come from `devflow exec`, so a repository without mise gets none.
        let uses_mise = facts.toolchain.mise.is_some();
        let mut proposed = 0;
        for candidate in candidates {
            let id = format!("daemon:{}", candidate.name);
            let runs_already = existing
                .runs
                .iter()
                .any(|run| normalized(run).starts_with(&normalized(&candidate.base)));
            if existing.names.contains(&candidate.name) || runs_already {
                self.satisfied.push(id);
                continue;
            }
            if proposed >= MAX_DAEMONS {
                self.warn(format!(
                    "more than {MAX_DAEMONS} processes were detected; only the first {MAX_DAEMONS} \
                     are proposed as daemons"
                ));
                break;
            }
            // The daemon gets the workspace's variables from `devflow exec`,
            // which sets them after mise (`mise = true`) and anything else
            // has set theirs. A mise task cannot be wrapped that way.
            let needs_workspace_env = self.needs_workspace_env();
            let wrapped = needs_workspace_env && !runs_mise_task(&candidate.run);
            let run = if wrapped {
                through_devflow_exec(&candidate.run)
            } else {
                candidate.run.clone()
            };
            let daemon =
                |key: &str| vec!["daemons".to_string(), candidate.name.clone(), key.into()];
            let mut ops = vec![
                PatchOp::Table {
                    path: vec!["daemons".into(), candidate.name.clone()],
                    comment: None,
                    dotted: false,
                },
                PatchOp::Set {
                    path: daemon("run"),
                    value: json!(run),
                    comment: None,
                },
            ];
            if let Some(dir) = &candidate.dir {
                ops.push(PatchOp::Set {
                    path: daemon("dir"),
                    value: json!(dir),
                    comment: None,
                });
            }
            if let Some(port) = candidate.port {
                ops.push(PatchOp::Set {
                    path: daemon("port"),
                    value: json!({"expect": [port], "bump": true}),
                    comment: None,
                });
            }
            if uses_mise {
                ops.push(PatchOp::Set {
                    path: daemon("mise"),
                    value: json!(true),
                    comment: None,
                });
            }
            let mut risks = candidate.risks.clone();
            risks.push(
                "pitchfork.toml is part of what `devflow trust` approves; the daemon runs \
                 whatever this command runs."
                    .to_string(),
            );
            if wrapped {
                risks.push(
                    "`devflow exec` puts the workspace's variables (.devflow/env) on the \
                     daemon after mise has set its own; `devflow` must be on the PATH of the \
                     process that starts pitchfork's daemons."
                        .to_string(),
                );
            }
            // A mise task only gets `.devflow/env` through mise; the plan
            // links it to the item that arranges that (see `link_mise_wiring`).
            let relies_on_mise_env = !wrapped && uses_mise && needs_workspace_env;
            let spec = Spec::new(
                id,
                ItemKind::Daemon,
                PITCHFORK_FILE,
                format!(
                    "Run `{}` as the pitchfork daemon `{}`",
                    candidate.base, candidate.name
                ),
            )
            .value(json!({
                "name": candidate.name,
                "run": run,
                "dir": candidate.dir,
                "port": candidate.port,
                "mise": uses_mise,
            }))
            .confidence(candidate.confidence)
            .evidence(candidate.evidence)
            .risks(risks)
            .ops(ops);
            let id = spec.id.clone();
            if self.push(spec) == Pushed::Item && relies_on_mise_env {
                self.mise_env_users.push(id);
            }
            proposed += 1;
        }
    }

    fn daemon_candidates(&mut self) -> Vec<Candidate> {
        let facts = self.facts;
        let processes: Vec<&ProcessFact> = facts
            .processes
            .iter()
            .filter(|p| p.source != ProcessSource::Pitchfork)
            .collect();
        let rails_bin_dev = processes
            .iter()
            .any(|p| p.source == ProcessSource::RailsBinDev);
        let workspace_dev = facts.js.iter().any(|project| {
            project.path == "."
                && project.workspaces.is_some()
                && project
                    .packages
                    .iter()
                    .find(|p| p.path == ".")
                    .is_some_and(|p| p.has_script("dev") || p.has_script("start"))
        });
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut notes: Vec<(String, String, Vec<String>)> = Vec::new();

        for process in processes {
            let launch_line = process.launch.as_ref().and_then(ProcessLaunch::shell_line);
            let dir = (process.cwd != ".").then(|| process.cwd.clone());
            match process.source {
                ProcessSource::RailsBinDev => candidates.push(Candidate {
                    name: web_name(&process.cwd),
                    base: "bin/dev".into(),
                    run: "bin/dev".into(),
                    dir,
                    port: Some(3000),
                    confidence: Confidence::High,
                    evidence: process.evidence.clone(),
                    risks: vec![
                        "bin/dev starts every process of Procfile.dev under foreman as one \
                         daemon; the Procfile entries are not proposed separately."
                            .into(),
                    ],
                    rank: 0,
                }),
                ProcessSource::DjangoRunserver => {
                    let Some(line) = launch_line else { continue };
                    candidates.push(Candidate {
                        name: web_name(&process.cwd),
                        base: line.clone(),
                        run: format!("{line} 127.0.0.1:$PORT"),
                        dir,
                        port: Some(8000),
                        confidence: Confidence::High,
                        evidence: process.evidence.clone(),
                        risks: Vec::new(),
                        rank: 0,
                    });
                }
                ProcessSource::Phoenix => {
                    let Some(line) = launch_line else { continue };
                    candidates.push(Candidate {
                        name: web_name(&process.cwd),
                        base: line.clone(),
                        run: line,
                        dir,
                        port: Some(4000),
                        confidence: Confidence::Medium,
                        evidence: process.evidence.clone(),
                        risks: vec![
                            "config/dev.exs usually hardcodes port 4000; read PORT there \
                             (`System.get_env(\"PORT\")`) or workspaces collide."
                                .into(),
                        ],
                        rank: 0,
                    });
                }
                ProcessSource::Composer => {
                    let Some(line) = launch_line else { continue };
                    candidates.push(Candidate {
                        name: if process.name == "dev" {
                            web_name(&process.cwd)
                        } else {
                            slug(&process.name)
                        },
                        base: line.clone(),
                        run: line,
                        dir,
                        port: None,
                        confidence: Confidence::Medium,
                        evidence: process.evidence.clone(),
                        risks: vec![
                            "`artisan serve` does not read PORT, so two workspaces running at \
                             once collide on port 8000."
                                .into(),
                        ],
                        rank: 0,
                    });
                }
                ProcessSource::SpringBoot => {
                    let Some(line) = launch_line else { continue };
                    candidates.push(Candidate {
                        name: web_name(&process.cwd),
                        base: line.clone(),
                        run: format!("SERVER_PORT=$PORT {line}"),
                        dir,
                        port: Some(8080),
                        confidence: Confidence::Medium,
                        evidence: process.evidence.clone(),
                        risks: vec![
                            "Spring Boot reads SERVER_PORT, not PORT; the command maps one to \
                             the other."
                                .into(),
                        ],
                        rank: 0,
                    });
                }
                ProcessSource::PackageJson => {
                    if let Some(candidate) =
                        self.package_daemon(process, launch_line, dir, workspace_dev)
                    {
                        candidates.push(candidate);
                    }
                }
                ProcessSource::MiseTask => {
                    let Some(line) = launch_line else { continue };
                    candidates.push(Candidate {
                        name: slug(&process.name),
                        base: line.clone(),
                        run: line,
                        dir,
                        port: None,
                        confidence: Confidence::Medium,
                        evidence: process.evidence.clone(),
                        risks: vec![
                            "The mise task's command is not inspected, and no port is wired."
                                .into(),
                        ],
                        rank: 2,
                    });
                }
                ProcessSource::Procfile => {
                    let covered = rails_bin_dev
                        && matches!(&process.launch, Some(ProcessLaunch::Procfile { file, .. }) if file.ends_with("Procfile.dev"));
                    if covered
                        || !matches!(
                            process.role,
                            ProcessRole::Dev | ProcessRole::Serve | ProcessRole::Worker
                        )
                    {
                        continue;
                    }
                    if let Some(ProcessLaunch::Procfile {
                        file,
                        process: name,
                    }) = &process.launch
                    {
                        notes.push((
                            slug(name),
                            format!("{file}: {name}"),
                            process.evidence.clone(),
                        ));
                    }
                }
                ProcessSource::Pitchfork => {}
            }
        }

        // One daemon per name: the strongest source wins.
        candidates.sort_by_key(|candidate| candidate.rank);
        let mut seen = BTreeSet::new();
        candidates.retain(|candidate| seen.insert(candidate.name.clone()));

        // Procfile entries nothing else covers are notes, not daemons.
        let file = PITCHFORK_FILE;
        for (name, entry, evidence) in notes {
            if name.is_empty() || seen.contains(&name) {
                continue;
            }
            let spec = Spec::new(
                format!("daemon:{name}"),
                ItemKind::Daemon,
                file,
                format!("Run the Procfile process `{entry}` as a pitchfork daemon"),
            )
            .value(json!({"name": name, "procfile": entry, "run": null}))
            .confidence(Confidence::Low)
            .evidence(evidence)
            .risks([
                "Procfile command lines are not copied into the plan (they often inline \
                 credentials). Copy the command into pitchfork.toml as `run`, or run it with \
                 your Procfile runner (`foreman start NAME`, `overmind start NAME`)."
                    .to_string(),
            ]);
            self.note(spec);
        }
        candidates
    }

    /// A daemon for a `package.json` script.
    fn package_daemon(
        &self,
        process: &ProcessFact,
        launch_line: Option<String>,
        dir: Option<String>,
        workspace_dev: bool,
    ) -> Option<Candidate> {
        let facts = self.facts;
        let line = launch_line?;
        let script = process.name.as_str();
        let in_workspace_member = facts.js.iter().any(|project| {
            project.workspaces.is_some()
                && project
                    .packages
                    .iter()
                    .any(|p| p.path == process.cwd && p.workspace_member)
        });
        if in_workspace_member && workspace_dev {
            // The root script already starts the workspace.
            return None;
        }
        let base_name = if process.cwd == "." {
            None
        } else {
            process.cwd.rsplit('/').next().map(slug)
        };
        let named = |suffix: Option<&str>, default: &str| -> String {
            match (&base_name, suffix) {
                (Some(base), Some(suffix)) => format!("{base}-{}", slug(suffix)),
                (Some(base), None) => base.clone(),
                (None, Some(suffix)) => slug(suffix),
                (None, None) => default.to_string(),
            }
        };
        let root_default = if workspace_dev && process.cwd == "." {
            "dev"
        } else {
            "web"
        };
        let (name, serves, rank) = match script {
            "dev" | "start:dev" | "serve:dev" => (named(None, root_default), true, 1),
            "start" | "serve" => (named(None, root_default), true, 3),
            "worker" | "start:worker" => (
                match &base_name {
                    Some(base) => format!("{base}-worker"),
                    None => "worker".to_string(),
                },
                false,
                1,
            ),
            other if other.starts_with("dev:") => {
                (named(Some(&other["dev:".len()..]), "dev"), false, 1)
            }
            other if other.starts_with("worker:") => (
                named(
                    Some(&format!("worker-{}", &other["worker:".len()..])),
                    "worker",
                ),
                false,
                1,
            ),
            _ => return None,
        };
        if name.is_empty() {
            return None;
        }
        let manager = facts
            .js
            .iter()
            .filter(|project| {
                project.path == "."
                    || project.path == process.cwd
                    || process.cwd.starts_with(&format!("{}/", project.path))
            })
            .max_by_key(|project| project.path.len())
            .map(|project| &project.package_manager)?;
        let framework: Option<&FrameworkFact> = facts
            .frameworks
            .iter()
            .find(|f| f.path == process.cwd && is_js_framework(f.framework));

        let mut run = line.clone();
        let mut port = None;
        let mut confidence = Confidence::Medium;
        let mut risks = Vec::new();
        let is_dev_server = serves && matches!(script, "dev" | "start:dev" | "serve:dev");
        if is_dev_server && !(workspace_dev && process.cwd == ".") {
            match framework {
                Some(f) if f.framework == Framework::NextJs => {
                    port = Some(3000);
                    confidence = Confidence::High;
                }
                Some(f) if f.vite_based && f.framework != Framework::Astro => {
                    run = forward(manager, &line, "--port $PORT --strictPort");
                    port = Some(5173);
                    confidence = Confidence::High;
                    risks.push(
                        "The port is passed with `--port $PORT --strictPort`; the script must \
                         forward its arguments to the Vite CLI."
                            .into(),
                    );
                }
                Some(f) if f.framework == Framework::Astro => {
                    run = forward(manager, &line, "--port $PORT");
                    port = Some(4321);
                    risks.push(
                        "The port is passed with `--port $PORT`; the script must forward its \
                         arguments to the Astro CLI."
                            .into(),
                    );
                }
                Some(f) if f.framework == Framework::NestJs => {
                    port = Some(3000);
                    risks.push(
                        "NestJS must listen on `process.env.PORT`; the default template \
                         hardcodes 3000."
                            .into(),
                    );
                }
                _ => risks.push(
                    "No framework with a known PORT convention was recognized: the script must \
                     read $PORT, or two workspaces started together collide."
                        .into(),
                ),
            }
        } else if workspace_dev && process.cwd == "." {
            risks.push(
                "The root script starts the whole workspace (often through turbo or a \
                 workspace filter); its apps need to read $PORT themselves."
                    .into(),
            );
        }
        let risk_script = format!(
            "The body of the package.json script `{script}` is not inspected; the daemon runs \
             `{line}`."
        );
        risks.push(risk_script);
        Some(Candidate {
            name,
            base: line,
            run,
            dir,
            port,
            confidence,
            evidence: process.evidence.clone(),
            risks,
            rank,
        })
    }

    fn existing_daemons(&self) -> ExistingDaemons {
        let mut names = BTreeSet::new();
        let mut runs = Vec::new();
        for file in [
            "pitchfork.toml",
            "pitchfork.local.toml",
            ".config/pitchfork.toml",
        ] {
            let Some(text) = self.text(file) else {
                continue;
            };
            let Ok(table) = text.parse::<toml::Table>() else {
                continue;
            };
            let Some(daemons) = table.get("daemons").and_then(toml::Value::as_table) else {
                continue;
            };
            for (name, daemon) in daemons {
                names.insert(name.clone());
                if let Some(run) = daemon.get("run").and_then(toml::Value::as_str) {
                    runs.push(run.to_string());
                }
            }
        }
        ExistingDaemons { names, runs }
    }
}

/// The daemon name of an application's main server: `web` at the root, the
/// directory's name for an application in a subdirectory.
fn web_name(cwd: &str) -> String {
    if cwd == "." {
        return "web".to_string();
    }
    let name = cwd.rsplit('/').next().map(slug).unwrap_or_default();
    if name.is_empty() {
        "web".to_string()
    } else {
        name
    }
}

fn is_js_framework(framework: Framework) -> bool {
    matches!(
        framework,
        Framework::NextJs
            | Framework::Vite
            | Framework::ReactRouter
            | Framework::Remix
            | Framework::SvelteKit
            | Framework::Astro
            | Framework::NestJs
    )
}

/// `run` with `args` forwarded to the script the way the package manager
/// wants it: npm needs `--`, the others pass trailing arguments through.
fn forward(manager: &PackageManagerFact, run: &str, args: &str) -> String {
    match manager.kind {
        PackageManager::Npm => format!("{run} -- {args}"),
        _ => format!("{run} {args}"),
    }
}

/// A command with `mise exec --`, `devflow exec --` and extra spaces
/// removed, for comparing.
fn normalized(run: &str) -> String {
    let mut words: Vec<&str> = run.split_whitespace().collect();
    loop {
        words = match words.as_slice() {
            ["mise", "exec", "--", rest @ ..]
            | ["mise", "x", "--", rest @ ..]
            | ["devflow", "exec", "--", rest @ ..] => rest.to_vec(),
            _ => break,
        };
    }
    // `SERVER_PORT=$PORT devflow exec -- cmd`: the assignments come first.
    let assignments = words.iter().take_while(|word| is_assignment(word)).count();
    if let ["devflow", "exec", "--", rest @ ..] = &words[assignments..] {
        let mut kept: Vec<&str> = words[..assignments].to_vec();
        kept.extend(rest);
        words = kept;
    }
    words.join(" ")
}

/// `run` with `devflow exec --` ahead of its command, after any leading
/// `NAME=value` assignments (`SERVER_PORT=$PORT devflow exec -- cmd`: the
/// shell expands and exports them, and devflow keeps what it inherits).
fn through_devflow_exec(run: &str) -> String {
    let mut prefix = String::new();
    let mut rest = run.trim_start();
    loop {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        if !is_assignment(word) {
            break;
        }
        prefix.push_str(word);
        prefix.push(' ');
        rest = rest[end..].trim_start();
    }
    format!("{prefix}devflow exec -- {rest}")
}

/// A mise task (`mise run NAME`, `mise watch`, ...) rather than a command
/// that `devflow exec` can wrap.
fn runs_mise_task(run: &str) -> bool {
    crate::env_file::mise_uses(run)
        .iter()
        .any(|found| found.runs_code && found.unfaithful == Some(crate::env_file::Unfaithful::Task))
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}
