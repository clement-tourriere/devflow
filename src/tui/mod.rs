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

    if let Some(workspace_name) = app.take_open_branch_on_exit() {
        let config = &app.context.config;
        let project_dir = app
            .context
            .config_path
            .as_ref()
            .and_then(|p: &std::path::PathBuf| p.parent().map(|d| d.to_path_buf()))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));

        let options = devflow_core::workspace::switch::SwitchOptions {
            lifecycle: devflow_core::workspace::LifecycleOptions::default(),
            create_if_missing: true,
            ..Default::default()
        };

        let result = devflow_core::workspace::switch::switch_workspace(
            config,
            &project_dir,
            &workspace_name,
            &options,
        )
        .await?;

        // Print results to stdout (terminal already restored)
        println!("Switched to workspace '{}'", result.workspace);
        if let Some(ref wt) = result.worktree {
            println!("Worktree: {}", wt.path.display());
        }
    }

    Ok(())
}
