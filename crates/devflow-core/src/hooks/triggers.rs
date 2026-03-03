use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::HookPhase;

/// Maps VCS events to devflow hook phases.
///
/// Default mapping:
/// ```text
/// git post-checkout  → [post-switch]  (or post-create if workspace is new)
/// git pre-commit     → [pre-commit]
/// git post-merge     → [post-merge, post-switch]
/// git post-rewrite   → [post-rewrite]
/// ```
///
/// Overridable via config:
/// ```yaml
/// triggers:
///   git:
///     post-checkout: [post-switch]
///     pre-commit: [pre-commit]
///     post-merge: [post-merge, post-switch]
///     post-rewrite: [post-rewrite]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggersConfig {
    #[serde(default)]
    pub git: HashMap<String, Vec<String>>,
}

impl Default for TriggersConfig {
    fn default() -> Self {
        Self {
            git: default_git_triggers(),
        }
    }
}

/// Default VCS-to-devflow phase mapping.
fn default_git_triggers() -> HashMap<String, Vec<String>> {
    let mut m = HashMap::new();
    m.insert(
        "post-checkout".to_string(),
        vec!["post-switch".to_string()],
    );
    m.insert("pre-commit".to_string(), vec!["pre-commit".to_string()]);
    m.insert(
        "post-merge".to_string(),
        vec!["post-merge".to_string(), "post-switch".to_string()],
    );
    m.insert(
        "post-rewrite".to_string(),
        vec!["post-rewrite".to_string()],
    );
    m
}

impl TriggersConfig {
    /// Resolve a VCS event name into a list of devflow `HookPhase`s.
    pub fn resolve_git_event(&self, event: &str) -> Vec<HookPhase> {
        self.git
            .get(event)
            .map(|phases| {
                phases
                    .iter()
                    .map(|s| s.parse::<HookPhase>().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return all configured VCS-to-phase mappings for display.
    pub fn git_mappings(&self) -> Vec<TriggerMapping> {
        let mut mappings: Vec<_> = self
            .git
            .iter()
            .map(|(event, phases)| TriggerMapping {
                vcs_event: event.clone(),
                phases: phases.clone(),
            })
            .collect();
        mappings.sort_by(|a, b| a.vcs_event.cmp(&b.vcs_event));
        mappings
    }
}

/// A single VCS event → devflow phases mapping (for display).
#[derive(Debug, Clone, Serialize)]
pub struct TriggerMapping {
    pub vcs_event: String,
    pub phases: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_triggers() {
        let triggers = TriggersConfig::default();

        let phases = triggers.resolve_git_event("post-checkout");
        assert_eq!(phases, vec![HookPhase::PostSwitch]);

        let phases = triggers.resolve_git_event("post-merge");
        assert_eq!(phases, vec![HookPhase::PostMerge, HookPhase::PostSwitch]);

        let phases = triggers.resolve_git_event("pre-commit");
        assert_eq!(phases, vec![HookPhase::PreCommit]);

        let phases = triggers.resolve_git_event("unknown-event");
        assert!(phases.is_empty());
    }

    #[test]
    fn test_custom_triggers() {
        let mut git = HashMap::new();
        git.insert(
            "post-checkout".to_string(),
            vec!["post-create".to_string(), "post-switch".to_string()],
        );
        let triggers = TriggersConfig { git };

        let phases = triggers.resolve_git_event("post-checkout");
        assert_eq!(phases, vec![HookPhase::PostCreate, HookPhase::PostSwitch]);
    }

    #[test]
    fn test_git_mappings_sorted() {
        let triggers = TriggersConfig::default();
        let mappings = triggers.git_mappings();
        let events: Vec<_> = mappings.iter().map(|m| m.vcs_event.as_str()).collect();
        assert!(events.windows(2).all(|w| w[0] <= w[1]));
    }
}
