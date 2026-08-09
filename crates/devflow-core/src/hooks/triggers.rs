use serde::Serialize;

/// A single VCS event → devflow phases mapping (for display).
#[derive(Debug, Clone, Serialize)]
pub struct TriggerMapping {
    pub vcs_event: String,
    pub phases: Vec<String>,
}

/// The fixed VCS-event → devflow-phase dispatch, for display (`devflow hook
/// triggers`, GUI hooks page).
///
/// The actual dispatch is not table-driven: the installed post-checkout hook
/// adopts a linked worktree via the switch pipeline (which runs post-create
/// for new workspaces, then post-switch), and the installed pre-commit hook
/// runs `devflow hook run pre-commit`. A configurable `triggers:` section
/// existed historically but was never consulted at runtime, so it was
/// removed rather than left as a decoy.
pub fn git_trigger_mappings() -> Vec<TriggerMapping> {
    vec![
        TriggerMapping {
            vcs_event: "post-checkout".to_string(),
            phases: vec![
                "post-create (new workspaces)".to_string(),
                "post-switch".to_string(),
            ],
        },
        TriggerMapping {
            vcs_event: "pre-commit".to_string(),
            phases: vec!["pre-commit".to_string()],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_mappings_sorted_and_complete() {
        let mappings = git_trigger_mappings();
        let events: Vec<_> = mappings.iter().map(|m| m.vcs_event.as_str()).collect();
        assert_eq!(events, vec!["post-checkout", "pre-commit"]);
        assert!(mappings.iter().all(|m| !m.phases.is_empty()));
    }
}
