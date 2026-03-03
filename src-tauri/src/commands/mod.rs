pub mod config;
pub mod hooks;
pub mod projects;
pub mod proxy;
pub mod services;
pub mod settings;
pub mod terminal;
pub mod workspaces;

/// Format an anyhow error with full cause chain, deduplicating adjacent identical messages.
pub fn format_error(err: anyhow::Error) -> String {
    let chain: Vec<String> = err.chain().map(|e| e.to_string()).collect();
    let mut parts: Vec<&str> = Vec::new();
    for msg in &chain {
        if parts.last().map(|p: &&str| *p == msg.as_str()).unwrap_or(false) {
            continue;
        }
        parts.push(msg.as_str());
    }
    parts.join(": ")
}
