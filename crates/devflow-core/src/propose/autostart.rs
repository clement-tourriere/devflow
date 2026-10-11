//! `[pitchfork] autostart`: starting a workspace's daemons once it is ready.
//!
//! Proposed with medium confidence, so it is off unless somebody accepts it:
//! starting processes is a decision, and it is still gated at run time by the
//! approval of the workspace's pitchfork files and by the absence of daemons
//! that start on their own.
use super::existing::PITCHFORK_FILE;
use super::plan::{Builder, Spec};
use super::{Confidence, ItemKind, PatchOp};
use crate::pitchfork_scope::ProcessesScope;
use serde_json::json;

impl Builder<'_> {
    pub fn autostart(&mut self) {
        let has_file = self.facts.wiring.pitchfork.is_some();
        let proposes_daemons = self
            .items
            .iter()
            .any(|item| item.kind == ItemKind::Daemon && item.is_actionable());
        if !has_file && !proposes_daemons {
            return;
        }
        let file = self.devflow_file();
        // Somebody already decided, either way.
        let decided = self
            .text(file)
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|table| table.get("pitchfork")?.get("autostart").cloned())
            .is_some();
        if decided {
            self.satisfied.push("pitchfork:autostart".into());
            return;
        }
        let mut evidence = Vec::new();
        if let Some(pitchfork) = &self.facts.wiring.pitchfork {
            evidence.extend(pitchfork.evidence.iter().cloned());
        }
        if evidence.is_empty() && proposes_daemons {
            evidence.push(format!("{PITCHFORK_FILE}: proposed daemons"));
        }
        let mut risks = vec![
            "Every `devflow setup` and `devflow switch` that finds the workspace ready runs \
             `pitchfork start` for its daemons, so they run with your permissions as soon \
             as you enter a workspace. They start only when the workspace is ready and its \
             pitchfork files are the ones `devflow trust` approved."
                .to_string(),
        ];
        let autonomous = ProcessesScope::read(self.root).autonomous;
        if !autonomous.is_empty() {
            let list: Vec<String> = autonomous
                .iter()
                .map(|t| format!("{} ({})", t.daemon, t.trigger))
                .collect();
            risks.push(format!(
                "{} start on their own under pitchfork (cron, watch, boot_start or auto), so \
                 devflow starts nothing in bulk while they are configured: autostart would do \
                 nothing until those triggers are removed.",
                list.join(", ")
            ));
        }
        let spec = Spec::new(
            "pitchfork:autostart",
            ItemKind::Daemon,
            file,
            "Start the workspace's daemons when it is ready ([pitchfork] autostart)",
        )
        .value(json!({"autostart": true}))
        .confidence(Confidence::Medium)
        .evidence(evidence)
        .risks(risks)
        .ops(vec![
            PatchOp::Table {
                path: vec!["pitchfork".into()],
                comment: Some(
                    "devflow starts the daemons of a ready, approved workspace after setup and \
                     switch."
                        .into(),
                ),
                dotted: false,
            },
            PatchOp::Set {
                path: vec!["pitchfork".into(), "autostart".into()],
                value: json!(true),
                comment: None,
            },
        ]);
        self.push(spec);
    }
}
