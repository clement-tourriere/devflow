use ratatui::style::{Color, Modifier, Style};

/// Spinner frames for loading indicators.
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Map a service state string to a color.
pub fn state_color(state: &str) -> Color {
    match state {
        "running" | "Running" => Color::Green,
        "stopped" | "Stopped" => Color::Yellow,
        "failed" | "Failed" => Color::Red,
        "Provisioning" => Color::Blue,
        _ => Color::DarkGray,
    }
}

// ── Semantic color constants ────────────────────────────────────────

// Borders
pub const BORDER_ACTIVE: Color = Color::Cyan;
pub const BORDER_INACTIVE: Color = Color::DarkGray;

// Tab bar
pub const TAB_ACTIVE: Color = Color::Cyan;
pub const TAB_INACTIVE: Color = Color::DarkGray;
pub const TAB_TITLE: Color = Color::Cyan;

// Text
pub const TEXT_PRIMARY: Color = Color::White;
pub const TEXT_SECONDARY: Color = Color::DarkGray;
pub const TEXT_MUTED: Color = Color::DarkGray;

// Branches
pub const BRANCH_CURRENT: Color = Color::Green;
pub const BRANCH_DEFAULT: Color = Color::Yellow;
pub const BRANCH_WORKTREE: Color = Color::Cyan;

// Tree drawing
pub const TREE_LINE: Color = Color::DarkGray;
pub const TREE_COLLAPSED: Color = Color::Yellow;

// Services
pub const SERVICE_TYPE: Color = Color::Blue;

// Data values
pub const VALUE_DATABASE: Color = Color::Blue;
pub const VALUE_PARENT: Color = Color::Magenta;
pub const VALUE_PATH: Color = Color::Cyan;

// YAML syntax highlighting
pub const YAML_KEY: Color = Color::Cyan;
pub const YAML_VALUE: Color = Color::Green;
pub const YAML_COMMENT: Color = Color::DarkGray;
pub const YAML_LIST: Color = Color::Yellow;

// Log levels
pub const LOG_ERROR: Color = Color::Red;
pub const LOG_WARN: Color = Color::Yellow;
pub const LOG_INFO: Color = Color::Green;
pub const LOG_DEBUG: Color = Color::DarkGray;

// Doctor checks
pub const CHECK_PASS: Color = Color::Green;
pub const CHECK_FAIL: Color = Color::Red;

// Status bar
pub const STATUS_ERROR_FG: Color = Color::White;
pub const STATUS_ERROR_BG: Color = Color::Red;
pub const STATUS_SUCCESS_FG: Color = Color::Green;
pub const STATUS_HINT_FG: Color = Color::DarkGray;
pub const STATUS_BAR_BG: Color = Color::DarkGray;

// Keybinding hints
pub const KEY_HINT: Color = Color::Yellow;

// Dialogs
pub const DIALOG_CONFIRM_BORDER: Color = Color::Yellow;
pub const DIALOG_INPUT_BORDER: Color = Color::Cyan;
pub const DIALOG_HELP_BORDER: Color = Color::Yellow;

// Hooks
pub const HOOK_NAME: Color = Color::Yellow;
pub const HOOK_COMMAND: Color = Color::Green;
pub const HOOK_CONDITION: Color = Color::Blue;
pub const HOOK_PHASE: Color = Color::Cyan;
pub const HOOK_BACKGROUND: Color = Color::Magenta;

// System tab sub-sections
pub const SUBSECTION_ACTIVE: Color = Color::Cyan;
pub const SUBSECTION_INACTIVE: Color = Color::DarkGray;

// Selection highlight
pub fn highlight_style() -> Style {
    Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

/// Status bar keybinding hints per tab.
pub fn tab_hints(tab_index: usize) -> &'static str {
    match tab_index {
        0 => "j/k:Navigate  n/p:Service  Enter:SvcAlign  o:Open  S/x:Svc  A/X:All  c:Create  d:Delete  Space:Expand  /:Filter  r:Refresh",
        1 => "j/k:Navigate  n/p:Workspace  S:Start  x:Stop  l:Logs  r:Refresh",
        2 => "j/k:Navigate  s:Start  x:Stop  r:Refresh",
        3 => "1:Config  2:Hooks  3:Doctor  4:Caps  j/k:Scroll  v/s:HookTpl  r:Refresh",
        4 => "f:Focus  /:Filter  j/k:Scroll  g/G:Top/Bottom  PgUp/PgDn:Page  r:Refresh",
        _ => "",
    }
}
