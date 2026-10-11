//! Trust prompts. Core decides what is trusted; this module only asks the
//! person at the terminal and turns the answer into an [`Approval`] of the
//! exact scope that was shown.
use super::{Ctx, ProjectInfo, output};
use anyhow::{Result, bail};
use clap::Args;
use devflow_core::trust::{self, Approval, Scope, TrustState};
use devflow_core::{Project, redact, worktree};
use inquire::InquireError;
use serde_json::json;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Everything one approval covers, as shown before approving: every command
/// of every hook phase (an empty phase says so) and the timeout, the `[env]`
/// recipes and presets, and the pitchfork configuration with the daemons
/// that start without devflow flagged. Credentials that happen to be written
/// into any of it are masked, like in every other output.
pub fn describe(scope: &Scope) -> String {
    let mut lines = Vec::new();
    for section in scope.sections() {
        match section.id {
            "setup" | "switch" | "teardown" => {
                lines.push(format!("{}:", section.id));
                if section.lines.is_empty() {
                    lines.push("  (none)".to_string());
                }
            }
            "timeout" => {
                lines.push(format!("timeout: {} per command", section.lines.join(", ")));
                continue;
            }
            "autonomous" => lines.push(
                "! pitchfork starts these without devflow, as soon as it sees the project:"
                    .to_string(),
            ),
            "env" => lines.push("env:".to_string()),
            _ => lines.push(format!("{}:", section.title.to_lowercase())),
        }
        let mark = if section.warning { "! " } else { "" };
        lines.extend(section.lines.iter().map(|line| format!("  {mark}{line}")));
    }
    redact::text(&lines.join("\n"))
}

/// Why the question is asked.
fn reason(previously_trusted: bool, format_changed: bool) -> &'static str {
    if format_changed {
        "devflow's approval format changed (it used to cover hooks only): review what this project runs again."
    } else if previously_trusted {
        "What this project asks to run (hooks, [env], pitchfork processes) changed since it was approved."
    } else {
        "This project's hooks, [env] and pitchfork processes have not been approved on this machine."
    }
}

/// Whether setting up a workspace executes or applies anything an approval
/// governs: setup commands (`hooks`), or `[env]` recipes and presets.
pub fn governs_setup(project: &Project, hooks: bool) -> bool {
    !project.config.env.is_empty() || (hooks && !project.config.hooks.setup.is_empty())
}

enum Choice {
    TrustAndRun,
    RunOnce,
    Skip,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Choice::TrustAndRun => "Trust and run",
            Choice::RunOnce => "Run once",
            Choice::Skip => "Skip them",
        })
    }
}

/// The directory whose scope a command in `ctx.cwd` is about: the workspace
/// containing it (a linked worktree has its own pitchfork files), else the
/// main checkout.
pub fn current_target(ctx: &Ctx, project: &Project) -> PathBuf {
    worktree::containing(project, &ctx.cwd)
        .ok()
        .flatten()
        .map_or_else(|| project.root.clone(), |workspace| workspace.path)
}

/// The directory of workspace `name`, else the main checkout (core reports
/// the unknown name itself).
pub fn target_of(project: &Project, name: Option<&str>) -> PathBuf {
    name.and_then(|name| worktree::find(project, name).ok().flatten())
        .map_or_else(|| project.root.clone(), |workspace| workspace.path)
}

/// Decide the approval for an operation that may run hooks (`relevant`) in
/// the workspace at `target` (or, for a new workspace, the one it is made
/// from): the scope shown and approved is that workspace's. The answer is
/// the [`Approval`] of exactly that scope, which core compares with the scope
/// of the workspace the operation acts on before running anything.
///
/// `DEVFLOW_TRUST_HOOKS=1` trusts for this run (core honours it itself, so
/// there is nothing to return). Otherwise an untrusted scope prompts when a
/// person is at the terminal; in every other case it returns `None` and core
/// reports the workspace as `needs_attention`.
pub fn resolve(
    ctx: &Ctx,
    project: &Project,
    target: &Path,
    relevant: bool,
) -> Result<Option<Approval>> {
    if !relevant || trust::env_override() {
        return Ok(None);
    }
    let TrustState::Untrusted {
        previously_trusted,
        format_changed,
        ..
    } = trust::state_at(project, target)?
    else {
        return Ok(None);
    };
    if !ctx.interactive() {
        return Ok(None);
    }
    let scope = trust::scope_at(project, target);
    output::note(format!(
        "{} The hooks run shell commands as you:\n{}",
        reason(previously_trusted, format_changed),
        describe(&scope)
    ));
    let options = vec![Choice::TrustAndRun, Choice::RunOnce, Choice::Skip];
    match inquire::Select::new("Run with this scope?", options).prompt() {
        Ok(Choice::TrustAndRun) => {
            // Approves exactly what was just shown, or fails if it changed.
            trust::approve_scope_at(project, target, &scope)?;
            Ok(Some(Approval::of(&scope)))
        }
        // Bound to what was shown; never stored.
        Ok(Choice::RunOnce) => Ok(Some(Approval::of(&scope))),
        Ok(Choice::Skip) => Ok(None),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            bail!("cancelled")
        }
        Err(error) => Err(error.into()),
    }
}

#[derive(Args)]
pub struct TrustArgs {
    /// Withdraw approval of this project's hooks, [env] and pitchfork processes
    #[arg(long)]
    pub revoke: bool,
    /// Approve without asking
    #[arg(short = 'y', long, conflicts_with = "revoke")]
    pub yes: bool,
}

/// `devflow trust`: show everything one approval covers and record approval.
pub fn command(ctx: &Ctx, args: TrustArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let target = current_target(ctx, &project);
    let scope = trust::scope_at(&project, &target);
    if args.revoke {
        let revoked = trust::revoke(&project)?;
        if ctx.json {
            output::emit(
                ctx,
                "trust",
                &json!({ "project": ProjectInfo::of(&project), "revoked": revoked }),
            )?;
        } else if revoked {
            output::say(
                "Revoked the approval of this project's hooks, [env] and pitchfork processes.",
            );
        } else {
            output::say("This project's scope was not approved.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let mut state = trust::state_at(&project, &target)?;
    let mut approved = false;
    let mut shown = false;
    if matches!(state, TrustState::Untrusted { .. }) {
        let approve = if args.yes {
            true
        } else if ctx.interactive() {
            output::say(describe(&scope));
            shown = true;
            match inquire::Confirm::new("Trust all of this?")
                .with_default(false)
                .prompt()
            {
                Ok(answer) => answer,
                Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
                    bail!("cancelled")
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            false
        };
        if approve {
            trust::approve_scope_at(&project, &target, &scope)?;
            state = trust::state_at(&project, &target)?;
            approved = true;
        }
    }

    if ctx.json {
        let body = json!({
            "project": ProjectInfo::of(&project),
            "path": target,
            "trust": state,
            "approved": approved,
            // The same scope, with credential-named `[env]` variables masked
            // like `env list` does; the digest is the real one.
            "scope": scope.review_json(),
        });
        output::emit(ctx, "trust", &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    match &state {
        TrustState::NothingToApprove => output::say(
            "Nothing to approve: no hooks, [env] or pitchfork processes are configured.",
        ),
        TrustState::Trusted => {
            if !shown {
                output::say(describe(&scope));
            }
            output::say(if approved {
                "Approved."
            } else {
                "This scope is trusted."
            });
        }
        TrustState::Untrusted {
            previously_trusted,
            format_changed,
            ..
        } => {
            if !shown {
                output::say(describe(&scope));
            }
            output::say(format!(
                "This scope is not trusted. {}",
                reason(*previously_trusted, *format_changed)
            ));
            output::say(
                "Run `devflow trust --yes` to approve it, or set DEVFLOW_TRUST_HOOKS=1 to trust it for one run.",
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use devflow_core::Config;
    use std::path::Path;

    const HEAD: &str =
        "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";

    fn scope_in(dir: &Path, extra: &str) -> Scope {
        Scope::build(&Config::parse(&format!("{HEAD}{extra}")).unwrap(), dir)
    }

    fn scope(extra: &str) -> Scope {
        scope_in(tempfile::tempdir().unwrap().path(), extra)
    }

    #[test]
    fn the_review_lists_every_phase_and_the_timeout() {
        let text = describe(&scope(
            "[hooks]\nsetup = [\"bun install\"]\nteardown = [\"make clean\", \"rm -rf tmp\"]\n",
        ));
        assert_eq!(
            text,
            "setup:\n  $ bun install\nswitch:\n  (none)\nteardown:\n  $ make clean\n  $ rm -rf tmp\ntimeout: 15m per command"
        );
    }

    #[test]
    fn named_entries_env_and_processes_are_part_of_the_review() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"bun run dev\"\nboot_start = true\n",
        )
        .unwrap();
        let text = describe(&scope_in(
            dir.path(),
            "[hooks]\nsetup = [{ name = \"migrate\", run = \"bun run db\", timeout = \"5m\" }]\n\
             [env]\npresets = [\"prisma\"]\nAUTH = \"${PGUSER}:${PGPASSWORD}\"\n",
        ));
        for needle in [
            "  [migrate] $ bun run db  (timeout 5m)",
            "env:\n  presets: prisma\n  AUTH = ${PGUSER}:${PGPASSWORD}",
            "processes (pitchfork.toml):\n  daemons.api\n    run: bun run dev\n    boot_start: true",
            "! pitchfork starts these without devflow, as soon as it sees the project:\n  ! api in pitchfork.toml: boot_start",
        ] {
            assert!(text.contains(needle), "{needle} missing:\n{text}");
        }
    }

    #[test]
    fn the_review_masks_credentials_written_into_any_part() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"psql postgres://u:hunter3@h/db\"\n", // kingfisher:ignore
        )
        .unwrap();
        let text = describe(&scope_in(
            dir.path(),
            "[hooks]\nsetup = [\"psql postgres://u:hunter2@h/db\"]\n", // kingfisher:ignore
        ));
        assert!(text.contains("postgres://u:***@h/db"), "{text}");
        assert!(
            !text.contains("hunter2") && !text.contains("hunter3"),
            "{text}"
        );
    }

    #[test]
    fn setup_is_governed_by_setup_hooks_and_env_not_by_the_rest_of_the_scope() {
        let project = |extra: &str| {
            let t = tempfile::tempdir().unwrap();
            std::fs::write(t.path().join("devflow.toml"), format!("{HEAD}{extra}")).unwrap();
            let repo = git2::Repository::init(t.path()).unwrap();
            let sig = git2::Signature::now("T", "t@example.com").unwrap();
            let tree = repo
                .find_tree(repo.index().unwrap().write_tree().unwrap())
                .unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "i", &tree, &[])
                .unwrap();
            (t.path().to_path_buf(), t)
        };
        let (a, _keep_a) = project("[hooks]\nteardown = [\"x\"]\nswitch = [\"y\"]\n");
        let (b, _keep_b) = project("[hooks]\nsetup = [\"x\"]\n");
        let (c, _keep_c) = project("[env]\nA = \"1\"\n");
        let open = |p: &std::path::PathBuf| Project::open(p).unwrap();
        assert!(
            !governs_setup(&open(&a), true),
            "teardown and switch are not setup"
        );
        assert!(governs_setup(&open(&b), true));
        assert!(
            !governs_setup(&open(&b), false),
            "--no-hooks leaves nothing"
        );
        assert!(
            governs_setup(&open(&c), false),
            "[env] applies without hooks"
        );
    }
}
