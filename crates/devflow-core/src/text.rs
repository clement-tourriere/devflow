//! Small helpers for sentences that carry a count.

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

#[cfg(test)]
mod tests {
    use super::*;

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
