//! Hook trust prompts. Core decides what is trusted; this module only asks the
//! person at the terminal and turns the answer into `trust_hooks`.
use super::{Ctx, ProjectInfo, output};
use anyhow::{Result, bail};
use clap::Args;
use devflow_core::trust::{self, Scope, TrustState};
use devflow_core::{Project, redact};
use inquire::InquireError;
use serde_json::json;
use std::fmt;
use std::process::ExitCode;

/// Everything one approval covers, as shown before approving: every command
/// of every phase (an empty phase says so) and the timeout. Credentials that
/// happen to be written into a command are masked, like in every other output.
pub fn describe(scope: &Scope) -> String {
    let mut lines = Vec::new();
    for (label, commands) in [
        ("setup", &scope.setup),
        ("switch", &scope.switch),
        ("teardown", &scope.teardown),
    ] {
        lines.push(format!("{label}:"));
        if commands.is_empty() {
            lines.push("  (none)".to_string());
        }
        lines.extend(commands.iter().map(|command| format!("  $ {command}")));
    }
    lines.push(format!("timeout: {} per command", scope.timeout));
    redact::text(&lines.join("\n"))
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
            Choice::Skip => "Skip hooks",
        })
    }
}

/// Decide `trust_hooks` for an operation that may run hooks (`relevant`).
///
/// `DEVFLOW_TRUST_HOOKS=1` trusts for this run. Otherwise an untrusted
/// `[hooks]` section prompts when a person is at the terminal; in every other
/// case it returns false and core reports the workspace as `needs_attention`.
pub fn resolve(ctx: &Ctx, project: &Project, relevant: bool) -> Result<bool> {
    if !relevant {
        return Ok(false);
    }
    if trust::env_override() {
        return Ok(true);
    }
    let TrustState::Untrusted {
        previously_trusted, ..
    } = trust::state(project)?
    else {
        return Ok(false);
    };
    if !ctx.interactive() {
        return Ok(false);
    }
    let reason = if previously_trusted {
        "The [hooks] section changed since it was approved."
    } else {
        "This project's [hooks] have not been approved on this machine."
    };
    let scope = trust::scope(project);
    output::note(format!(
        "{reason} They run shell commands as you:\n{}",
        describe(&scope)
    ));
    let options = vec![Choice::TrustAndRun, Choice::RunOnce, Choice::Skip];
    match inquire::Select::new("Run these hooks?", options).prompt() {
        Ok(Choice::TrustAndRun) => {
            // Approves exactly what was just shown, or fails if it changed.
            trust::approve_scope(project, &scope)?;
            Ok(true)
        }
        Ok(Choice::RunOnce) => Ok(true),
        Ok(Choice::Skip) => Ok(false),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            bail!("cancelled")
        }
        Err(error) => Err(error.into()),
    }
}

#[derive(Args)]
pub struct TrustArgs {
    /// Withdraw approval of this project's hooks
    #[arg(long)]
    pub revoke: bool,
    /// Approve without asking
    #[arg(short = 'y', long, conflicts_with = "revoke")]
    pub yes: bool,
}

/// `devflow trust`: show the hook commands and record approval.
pub fn command(ctx: &Ctx, args: TrustArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let scope = trust::scope(&project);
    if args.revoke {
        let revoked = trust::revoke(&project)?;
        if ctx.json {
            output::emit(
                ctx,
                "trust",
                &json!({ "project": ProjectInfo::of(&project), "revoked": revoked }),
            )?;
        } else if revoked {
            output::say("Revoked approval of this project's hooks.");
        } else {
            output::say("These hooks were not approved.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let mut state = trust::state(&project)?;
    let mut approved = false;
    let mut shown = false;
    if matches!(state, TrustState::Untrusted { .. }) {
        let approve = if args.yes {
            true
        } else if ctx.interactive() {
            output::say(describe(&scope));
            shown = true;
            match inquire::Confirm::new("Trust these hooks?")
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
            trust::approve_scope(&project, &scope)?;
            state = trust::state(&project)?;
            approved = true;
        }
    }

    if ctx.json {
        let body = json!({
            "project": ProjectInfo::of(&project),
            "trust": state,
            "approved": approved,
            "scope": scope,
        });
        output::emit(ctx, "trust", &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    match &state {
        TrustState::NoHooks => output::say("No hooks are configured."),
        TrustState::Trusted => {
            if !shown {
                output::say(describe(&scope));
            }
            output::say(if approved {
                "Approved."
            } else {
                "These hooks are trusted."
            });
        }
        TrustState::Untrusted {
            previously_trusted, ..
        } => {
            if !shown {
                output::say(describe(&scope));
            }
            if *previously_trusted {
                output::say("The hooks changed since they were approved and are not trusted.");
            } else {
                output::say("These hooks are not trusted.");
            }
            output::say(
                "Run `devflow trust --yes` to approve them, or set DEVFLOW_TRUST_HOOKS=1 to trust them for one run.",
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(setup: &[&str], switch: &[&str], teardown: &[&str]) -> Scope {
        let list = |commands: &[&str]| commands.iter().map(|c| c.to_string()).collect();
        Scope {
            setup: list(setup),
            switch: list(switch),
            teardown: list(teardown),
            timeout: "15m".into(),
            digest: "0".repeat(64),
        }
    }

    #[test]
    fn the_review_lists_every_phase_and_the_timeout() {
        let text = describe(&scope(&["bun install"], &[], &["make clean", "rm -rf tmp"]));
        assert_eq!(
            text,
            "setup:\n  $ bun install\nswitch:\n  (none)\nteardown:\n  $ make clean\n  $ rm -rf tmp\ntimeout: 15m per command"
        );
    }

    #[test]
    fn the_review_masks_credentials_written_into_commands() {
        let text = describe(&scope(&["psql postgres://u:hunter2@h/db"], &[], &[])); // kingfisher:ignore
        assert!(text.contains("postgres://u:***@h/db"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }
}
