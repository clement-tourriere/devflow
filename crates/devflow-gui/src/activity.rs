//! Live output of the running (or last) workspace operation.
use crate::{
    jobs::{Done, Event, MAX_LINE_BYTES, clip},
    model::{RequestKey, Section},
};
use devflow_core::receipt::{Phase, PhaseState};
use std::collections::VecDeque;

/// Output lines kept for display; older ones are dropped.
pub const MAX_LINES: usize = 2000;
/// Bytes kept for display; the oldest lines go first.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

pub struct LogBuffer {
    lines: VecDeque<String>,
    bytes: usize,
    cap: usize,
    max_bytes: usize,
    revision: u64,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self::with_limits(cap, MAX_BYTES)
    }

    pub fn with_limits(cap: usize, max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            cap,
            max_bytes,
            revision: 0,
        }
    }

    pub fn push_batch(&mut self, batch: impl IntoIterator<Item = String>) {
        let mut changed = false;
        for line in batch {
            let line = if line.len() > MAX_LINE_BYTES {
                clip(&line, MAX_LINE_BYTES)
            } else {
                line
            };
            // One byte for the newline that joins it to the next line.
            self.bytes += line.len() + 1;
            self.lines.push_back(line);
            while self.lines.len() > 1
                && (self.lines.len() > self.cap || self.bytes > self.max_bytes)
            {
                if let Some(old) = self.lines.pop_front() {
                    self.bytes -= old.len() + 1;
                }
            }
            changed = true;
        }
        if changed {
            self.revision += 1;
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Bytes retained (text plus separators).
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Changes whenever the text does, so views can skip redundant updates.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub struct Activity {
    pub key: RequestKey,
    pub title: String,
    pub phases: Vec<(Phase, PhaseState, Option<String>)>,
    pub log: LogBuffer,
    /// `None` while running. The text is already credential-free.
    pub outcome: Option<Section<Done>>,
    /// When the outcome arrived.
    pub finished_at: Option<chrono::DateTime<chrono::Local>>,
    /// What core says can be done about a failure (buttons under its message).
    pub remedies: Vec<devflow_core::remedy::Remedy>,
}

impl Activity {
    pub fn new(key: RequestKey, title: impl Into<String>) -> Self {
        Self {
            key,
            title: title.into(),
            phases: Phase::ALL
                .iter()
                .map(|p| (*p, PhaseState::Pending, None))
                .collect(),
            log: LogBuffer::new(MAX_LINES),
            outcome: None,
            finished_at: None,
            remedies: Vec::new(),
        }
    }

    /// Apply one batch of progress events in order. `Finished` is handled by
    /// the caller because it also releases the project's mutation lock.
    pub fn apply(&mut self, events: impl IntoIterator<Item = Event>) {
        let mut lines = Vec::new();
        for event in events {
            match event {
                Event::Phase {
                    phase,
                    state,
                    detail,
                } => {
                    lines.push(match &detail {
                        Some(d) => format!("[{}] {}: {d}", phase.as_str(), state_label(state)),
                        None => format!("[{}] {}", phase.as_str(), state_label(state)),
                    });
                    if let Some(slot) = self.phases.iter_mut().find(|(p, ..)| *p == phase) {
                        slot.1 = state;
                        slot.2 = detail;
                    }
                }
                Event::Line(line) => lines.push(line),
                Event::Finished(_) => {}
            }
        }
        self.log.push_batch(lines);
    }
}

pub fn state_label(state: PhaseState) -> &'static str {
    match state {
        PhaseState::Pending => "pending",
        PhaseState::Started => "started",
        PhaseState::Done => "done",
        PhaseState::Skipped => "skipped",
        PhaseState::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn activity() -> Activity {
        Activity::new(
            RequestKey {
                project: Uuid::new_v4(),
                epoch: 1,
            },
            "Create workspace",
        )
    }

    #[test]
    fn log_buffer_keeps_only_the_last_lines() {
        let mut log = LogBuffer::new(MAX_LINES);
        log.push_batch((0..2500).map(|i| format!("line {i}")));
        assert_eq!(log.len(), MAX_LINES);
        assert!(log.text().starts_with("line 500\n"));
        assert!(log.text().ends_with("line 2499"));
    }

    #[test]
    fn revision_only_moves_when_text_changes() {
        let mut log = LogBuffer::new(3);
        let r0 = log.revision();
        log.push_batch(Vec::new());
        assert_eq!(log.revision(), r0);
        log.push_batch(["a".to_string()]);
        assert!(log.revision() > r0);
    }

    #[test]
    fn events_update_phases_and_log_in_order() {
        let mut a = activity();
        a.apply([
            Event::Phase {
                phase: Phase::Services,
                state: PhaseState::Started,
                detail: None,
            },
            Event::Line("npm install".into()),
            Event::Phase {
                phase: Phase::Services,
                state: PhaseState::Failed,
                detail: Some("server down".into()),
            },
        ]);
        let db = a
            .phases
            .iter()
            .find(|(p, ..)| *p == Phase::Services)
            .unwrap();
        assert_eq!(db.1, PhaseState::Failed);
        assert_eq!(db.2.as_deref(), Some("server down"));
        let text = a.log.text();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "[services] started");
        assert_eq!(lines[1], "npm install");
        assert_eq!(lines[2], "[services] failed: server down");
        assert!(a.outcome.is_none());
    }

    #[test]
    fn log_buffer_is_bounded_in_bytes_not_only_in_lines() {
        let mut log = LogBuffer::with_limits(MAX_LINES, 10 * 1024);
        // 200 lines of ~1 KiB would be 200 KiB; only the newest ~10 KiB stay.
        log.push_batch((0..200).map(|i| format!("{i:04}{}", "x".repeat(1000))));
        assert!(log.bytes() <= 10 * 1024, "{} bytes kept", log.bytes());
        assert!(log.len() < 15);
        assert!(log.text().ends_with(&format!("0199{}", "x".repeat(1000))));
        assert!(!log.text().contains("0000"), "oldest lines were dropped");
    }

    #[test]
    fn a_single_huge_line_is_truncated_on_a_char_boundary() {
        let mut log = LogBuffer::new(MAX_LINES);
        // 3-byte characters: the cut must not split one.
        log.push_batch(["€".repeat(10_000)]);
        let text = log.text();
        assert!(text.len() < MAX_LINE_BYTES + 64, "{} bytes", text.len());
        assert!(text.ends_with("… (line truncated)"));
        assert!(text.starts_with('€'));
        // A line that exactly fits is kept whole.
        let mut log = LogBuffer::new(MAX_LINES);
        log.push_batch(["y".repeat(MAX_LINE_BYTES)]);
        assert_eq!(log.text().len(), MAX_LINE_BYTES);
    }

    #[test]
    fn the_newest_line_is_kept_even_if_it_alone_exceeds_the_byte_cap() {
        let mut log = LogBuffer::with_limits(10, 8);
        log.push_batch(["first".to_string(), "second line".to_string()]);
        assert_eq!(log.text(), "second line");
    }
}
