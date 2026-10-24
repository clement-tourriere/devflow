//! Small helpers for sentences that carry a count or a time.

/// `1 command`, `2 commands`, `0 commands`: `noun` with a regular `s`.
pub fn counted(n: u64, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// `1 entry`, `2 entries`: the singular and the plural spelled out, for the
/// nouns whose plural is not a regular `s`.
pub fn counted_as(n: u64, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

/// How long ago `at` was, in the biggest whole unit: `just now`, `5m ago`,
/// `3h ago`, `2d ago`, `3w ago`, `4mo ago`.
pub fn ago(at: chrono::DateTime<chrono::Utc>) -> String {
    let seconds = (chrono::Utc::now() - at).num_seconds().max(0);
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        86_400..=1_209_599 => format!("{}d ago", seconds / 86_400),
        1_209_600..=5_183_999 => format!("{}w ago", seconds / 604_800),
        _ => format!("{}mo ago", seconds / 2_592_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_told_in_the_biggest_whole_unit() {
        let at = |seconds: i64| ago(chrono::Utc::now() - chrono::Duration::seconds(seconds));
        assert_eq!(at(5), "just now");
        assert_eq!(at(5 * 60), "5m ago");
        assert_eq!(at(3 * 3600), "3h ago");
        assert_eq!(at(3 * 86_400), "3d ago");
        assert_eq!(at(21 * 86_400), "3w ago");
        assert_eq!(at(120 * 86_400), "4mo ago");
    }

    #[test]
    fn one_is_singular_and_everything_else_plural() {
        assert_eq!(counted(0, "command"), "0 commands");
        assert_eq!(counted(1, "command"), "1 command");
        assert_eq!(counted(2, "command"), "2 commands");
        assert_eq!(counted_as(1, "entry", "entries"), "1 entry");
        assert_eq!(counted_as(0, "entry", "entries"), "0 entries");
        assert_eq!(counted_as(12, "entry", "entries"), "12 entries");
    }
}
