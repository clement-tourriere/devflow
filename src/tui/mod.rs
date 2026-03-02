mod action;
mod app;
mod components;
mod context;
mod event;
mod theme;

use anyhow::Result;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;

use app::App;
use context::DevflowContext;

/// Entry point for `devflow tui`.
///
/// Sets up the terminal (raw mode, alternate screen), creates the
/// DevflowContext and App, runs the event loop, and restores the
/// terminal on exit (including on panic).
pub async fn run() -> Result<()> {
    // Build the shared data context before touching the terminal so that
    // config / VCS detection errors surface normally.
    let ctx = DevflowContext::new()?;

    // ── Terminal setup ──────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // ── Install a panic hook that restores the terminal ─────────────
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    // ── Run the application ─────────────────────────────────────────
    let mut app = App::new(ctx);
    let run_result = app.run(&mut terminal).await;

    // ── Restore terminal ────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    run_result?;

    if let Some(branch_name) = app.take_open_branch_on_exit() {
        let exe = std::env::current_exe()?;
        let status = tokio::process::Command::new(exe)
            .arg("switch")
            .arg(&branch_name)
            .status()
            .await?;
        if !status.success() {
            anyhow::bail!(
                "Failed to open branch '{}' from TUI (switch exited with code {})",
                branch_name,
                status.code().unwrap_or(-1)
            );
        }
    }

    Ok(())
}
