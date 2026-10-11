//! Environment commands (doctor, completions, gui) and process helpers.
use super::output::{self, Palette, note, say};
use super::{Ctx, ProjectInfo};
use anyhow::{Result, anyhow, bail};
use clap::{Args, CommandFactory};
use clap_complete::Shell;
use devflow_core::doctor::{self, Check, CheckStatus};
use devflow_core::error::NotInitialized;
use devflow_core::{Project, redact};
use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

/// First executable named `name` on `PATH`.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Replace this process with `command` (Unix) or run it and mirror its exit code.
pub fn exec(command: &mut Command) -> Result<ExitCode> {
    let program = command.get_program().to_string_lossy().into_owned();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(anyhow!("cannot run {program}: {error}"))
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .map_err(|error| anyhow!("cannot run {program}: {error}"))?;
        Ok(ExitCode::from(
            status.code().unwrap_or(1).clamp(0, 255) as u8
        ))
    }
}

// --- doctor -----------------------------------------------------------------

pub fn doctor(ctx: &Ctx) -> Result<ExitCode> {
    let mut checks = Vec::new();
    let project = match Project::discover(&ctx.cwd) {
        Ok(project) => Some(project),
        Err(error) => {
            let hint = if error.downcast_ref::<NotInitialized>().is_some() {
                "run `devflow init` to create devflow.toml"
            } else {
                "run devflow inside a Git repository (`devflow init` creates devflow.toml)"
            };
            checks.push(Check {
                name: "project".into(),
                status: CheckStatus::Warn,
                detail: format!("{error:#}"),
                hint: Some(hint.into()),
            });
            None
        }
    };
    checks.extend(doctor::run(project.as_ref()));
    let failed = checks.iter().any(|check| check.status == CheckStatus::Fail);
    if ctx.json {
        let body = json!({
            "project": project.as_ref().map(ProjectInfo::of),
            "ok": !failed,
            "checks": checks,
        });
        output::emit(ctx, "doctor", &body)?;
    } else {
        print_checks(&checks);
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn print_checks(checks: &[Check]) {
    let palette = Palette::stdout();
    let width = checks
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    for check in checks {
        let symbol = match check.status {
            CheckStatus::Ok => palette.green("✓"),
            CheckStatus::Warn => palette.yellow("!"),
            CheckStatus::Fail => palette.red("✗"),
        };
        say(redact::text(&format!(
            "{symbol} {:<width$}  {}",
            check.name, check.detail
        )));
        if let Some(hint) = &check.hint
            && check.status != CheckStatus::Ok
        {
            say(format!(
                "  {}  {}",
                " ".repeat(width),
                palette.dim(&redact::text(&format!("hint: {hint}")))
            ));
        }
    }
}

// --- completions ---------------------------------------------------------------

#[derive(Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    pub shell: Shell,
}

pub fn completions<C: CommandFactory>(args: CompletionsArgs) -> Result<ExitCode> {
    let mut buffer = Vec::new();
    clap_complete::generate(args.shell, &mut C::command(), "devflow", &mut buffer);
    output::say_raw(String::from_utf8_lossy(&buffer));
    Ok(ExitCode::SUCCESS)
}

// --- gui -----------------------------------------------------------------------

const GUI_BINARY: &str = "devflow-gui";

pub fn gui(ctx: &Ctx) -> Result<ExitCode> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(GUI_BINARY)))
        .filter(|path| is_executable(path));
    let Some(binary) = beside.or_else(|| which(GUI_BINARY)) else {
        bail!(
            "{GUI_BINARY} was not found next to devflow or on PATH; \
             install the desktop app or build it with `cargo build -p devflow-gui`"
        );
    };
    let mut command = Command::new(binary);
    if let Ok(project) = ctx.project() {
        command.arg("--project").arg(&project.root);
    } else {
        note("devflow: not inside a project; opening the app without one");
    }
    exec(&mut command)
}
