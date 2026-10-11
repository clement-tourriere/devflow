//! How completely a count was read, said the same way everywhere.
//!
//! A listing that failed shows nothing, and a fallback ("not started", zero
//! running) is not what was observed. The overview's tiles, the Data tab's
//! cards, the service page and the workspace detail all word a count through
//! [`Reading`], so a state that could not be read is never drawn as an exact
//! number and always says why.
use super::ui::Tone;

/// What a count rests on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading {
    /// Everything it counts was read.
    Known,
    /// Part of it could not be read: the count is what the rest shows, so at
    /// least that many.
    Partial {
        unread: usize,
        total: usize,
        reason: String,
    },
    /// None of it could be read: there is no count.
    Unknown { reason: String },
}

impl Reading {
    /// The value of a pill or tile: `3 running`, `at least 3 running`, or
    /// `unknown`.
    pub fn count_text(&self, count: usize, what: &str) -> String {
        match self {
            Self::Known => format!("{count} {what}"),
            Self::Partial { .. } => format!("at least {count} {what}"),
            Self::Unknown { .. } => "unknown".to_string(),
        }
    }

    /// Green for something that runs, gray for nothing, amber for anything
    /// that is not fully known.
    pub fn tone(&self, count: usize) -> Tone {
        match self {
            Self::Known if count > 0 => Tone::Success,
            Self::Known => Tone::Neutral,
            Self::Partial { .. } | Self::Unknown { .. } => Tone::Warning,
        }
    }

    /// The reason under the value; `None` when everything was read. `subject`
    /// is what failed to answer ("pitchfork", "Docker") and `unit` what the
    /// unread part is made of ("workspaces", "branches").
    pub fn note(&self, subject: &str, unit: &str) -> Option<String> {
        match self {
            Self::Known => None,
            Self::Partial {
                unread,
                total,
                reason,
            } => Some(format!(
                "{unread} of {total} {unit} could not be read: {reason}"
            )),
            Self::Unknown { reason } => Some(format!("{subject} could not be read: {reason}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_is_exact_only_when_everything_was_read() {
        let partial = Reading::Partial {
            unread: 1,
            total: 3,
            reason: "docker said no".into(),
        };
        let unknown = Reading::Unknown {
            reason: "docker said no".into(),
        };
        assert_eq!(Reading::Known.count_text(2, "running"), "2 running");
        assert_eq!(partial.count_text(2, "running"), "at least 2 running");
        assert_eq!(unknown.count_text(2, "running"), "unknown");
        assert!(matches!(Reading::Known.tone(0), Tone::Neutral));
        assert!(matches!(Reading::Known.tone(2), Tone::Success));
        assert!(matches!(partial.tone(2), Tone::Warning));
        assert!(matches!(unknown.tone(0), Tone::Warning));
        assert_eq!(Reading::Known.note("Docker", "branches"), None);
        assert_eq!(
            partial.note("Docker", "branches").unwrap(),
            "1 of 3 branches could not be read: docker said no"
        );
        assert_eq!(
            unknown.note("Docker", "branches").unwrap(),
            "Docker could not be read: docker said no"
        );
    }
}
