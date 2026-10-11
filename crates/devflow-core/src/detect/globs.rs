//! Workspace glob expansion over directories, bounded in depth, result count
//! and total work.
//!
//! Supports the subset package managers use for workspace members: literal
//! segments, `*`, `?`, `**`, `{a,b}` alternatives and `!` exclusions. Only
//! directories are matched, `node_modules` and hidden directories are never
//! entered by a wildcard, and wildcards expand at most [`MAX_WILDCARD_DEPTH`]
//! levels below the base.
//!
//! Patterns come from untrusted manifests, so nothing here recurses and every
//! step has a limit: brace expansion is an iterative worklist bounded by
//! pattern length, nesting depth, alternatives and scanned bytes (a pattern
//! over a limit is ignored with a warning), and the directory walk is an
//! explicit stack that charges every visit to the scan's shared work budget
//! (see `Repo::charge`), capped per expansion, and stops at a result cap. A
//! walk that stops early keeps what it found and says so in a warning.
use super::repo::{Repo, is_plain_relative, join};
use std::collections::BTreeSet;

/// Directory levels a wildcard (`*` or `**`) may descend.
pub(crate) const MAX_WILDCARD_DEPTH: usize = 2;
/// Most directories one expansion returns.
pub(crate) const MAX_RESULTS: usize = 300;
/// Most patterns of one declaration that are looked at.
const MAX_PATTERNS: usize = 256;
/// Longest pattern, in bytes.
const MAX_PATTERN_BYTES: usize = 512;
/// Deepest `{` nesting.
const MAX_BRACE_DEPTH: usize = 8;
/// Most alternatives one pattern expands to.
const MAX_ALTERNATIVES: usize = 32;
/// Most bytes scanned while expanding braces in one pattern.
const MAX_BRACE_WORK: usize = 64 * 1024;
/// Most filesystem work (see `Repo::charge`) one expansion may spend, so a
/// hostile workspace pattern cannot use up the budget the other detectors need.
const MAX_EXPANSION_WORK: u64 = 30_000;

/// Why braces were not expanded.
#[derive(Debug, PartialEq, Eq)]
enum BraceLimit {
    Length,
    Nesting,
    Alternatives,
    Work,
}

impl BraceLimit {
    fn describe(&self) -> &'static str {
        match self {
            BraceLimit::Length => "is too long",
            BraceLimit::Nesting => "nests braces too deeply",
            BraceLimit::Alternatives => "expands to too many alternatives",
            BraceLimit::Work => "is too expensive to expand",
        }
    }
}

/// Directories below `base` matching `patterns`, sorted. Patterns starting
/// with `!` remove matches. Patterns escaping the base, or exceeding a limit,
/// are ignored with a warning.
pub(crate) fn expand(repo: &Repo, base: &str, patterns: &[String]) -> Vec<String> {
    let mut included = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    // This expansion may spend this much of the scan's work budget.
    let ceiling = repo.work_spent().saturating_add(MAX_EXPANSION_WORK);
    if patterns.len() > MAX_PATTERNS {
        repo.warn(
            base,
            format!("only the first {MAX_PATTERNS} workspace patterns were used"),
        );
    }
    for pattern in patterns.iter().take(MAX_PATTERNS) {
        let (negated, pattern) = match pattern.trim().strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, pattern.trim()),
        };
        let alternatives = match braces(pattern) {
            Ok(alternatives) => alternatives,
            Err(limit) => {
                repo.warn(
                    base,
                    format!(
                        "workspace pattern '{}' {}; ignored",
                        clip(pattern),
                        limit.describe()
                    ),
                );
                continue;
            }
        };
        for alternative in alternatives {
            let normalized = alternative
                .trim_start_matches("./")
                .trim_end_matches('/')
                .to_string();
            if normalized.is_empty()
                || normalized.starts_with('/')
                || !is_plain_relative(&normalized)
            {
                repo.warn(
                    base,
                    format!(
                        "workspace pattern '{}' is outside the repository; ignored",
                        clip(pattern)
                    ),
                );
                continue;
            }
            let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
            let target = if negated {
                &mut excluded
            } else {
                &mut included
            };
            let stopped = walk(repo, base, &segments, target, ceiling);
            if let Some(reason) = stopped {
                repo.warn(
                    base,
                    format!(
                        "workspace pattern '{}' {}; only some directories are used",
                        clip(pattern),
                        reason.describe()
                    ),
                );
            }
        }
    }
    let mut dirs: Vec<String> = included
        .into_iter()
        .filter(|d| !excluded.contains(d))
        .collect();
    if dirs.len() > MAX_RESULTS {
        repo.warn(
            base,
            format!("only the first {MAX_RESULTS} workspace directories were used"),
        );
        dirs.truncate(MAX_RESULTS);
    }
    dirs
}

/// A pattern shortened for a warning.
fn clip(pattern: &str) -> String {
    pattern.chars().take(80).collect()
}

/// Why a directory walk stopped before it was done.
enum Stop {
    /// It found more directories than any workspace needs.
    Matches,
    /// It spent its share of the scan's work budget, or the scan's budget ran out.
    Work,
}

impl Stop {
    fn describe(&self) -> &'static str {
        match self {
            Stop::Matches => "matched too many directories",
            Stop::Work => "visited too many directories",
        }
    }
}

/// Collect the directories under `base` matching `segments` into `out`,
/// stopping early at too many matches or once the scan's work counter passes
/// `ceiling` (or its budget runs out).
fn walk(
    repo: &Repo,
    base: &str,
    segments: &[&str],
    out: &mut BTreeSet<String>,
    ceiling: u64,
) -> Option<Stop> {
    // (directory, index of the next segment, wildcard levels still allowed)
    let mut pending = vec![(base.to_string(), 0usize, MAX_WILDCARD_DEPTH)];
    while let Some((dir, at, budget)) = pending.pop() {
        if out.len() >= MAX_RESULTS * 4 {
            return Some(Stop::Matches);
        }
        if repo.work_spent() > ceiling || !repo.charge(1) {
            return Some(Stop::Work);
        }
        let Some(segment) = segments.get(at) else {
            if dir != "." {
                out.insert(dir);
            }
            continue;
        };
        if *segment == "**" {
            // One more level at a time, after zero levels.
            if budget > 0 {
                for child in wildcard_children(repo, &dir, "*").into_iter().rev() {
                    pending.push((child, at, budget - 1));
                }
            }
            pending.push((dir, at + 1, budget));
        } else if segment.contains(['*', '?']) {
            if budget > 0 {
                for child in wildcard_children(repo, &dir, segment).into_iter().rev() {
                    pending.push((child, at + 1, budget - 1));
                }
            }
        } else {
            let child = join(&dir, segment);
            if repo.is_dir(&child) {
                pending.push((child, at + 1, budget));
            }
        }
    }
    None
}

/// Subdirectories of `dir` whose name matches `pattern`.
fn wildcard_children(repo: &Repo, dir: &str, pattern: &str) -> Vec<String> {
    let index = repo.list(dir);
    index
        .dirs()
        .filter(|name| *name != "node_modules")
        .filter(|name| !name.starts_with('.') || pattern.starts_with('.'))
        .filter(|name| matches(pattern, name))
        .map(|name| join(dir, name))
        .collect()
}

/// `*` and `?` matching within one path segment.
pub(crate) fn matches(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Expand `{a,b}` alternatives, nested ones included, with a worklist instead
/// of recursion. A brace pair without a top-level comma (`{}`, `{a}`) is
/// literal text, as in shells. An unmatched `{` is literal too.
///
/// Results come out left to right (`{a,b}/{c,d}` is `a/c, a/d, b/c, b/d`).
fn braces(pattern: &str) -> Result<Vec<String>, BraceLimit> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(BraceLimit::Length);
    }
    let mut work = 0usize;
    let mut done: Vec<String> = Vec::new();
    // (text, offset to resume scanning at); popped from the end, so
    // alternatives are pushed in reverse to keep their order.
    let mut pending: Vec<(String, usize)> = vec![(pattern.to_string(), 0)];
    while let Some((text, from)) = pending.pop() {
        let Some(group) = next_group(&text, from, &mut work)? else {
            done.push(text);
            if done.len() + pending.len() > MAX_ALTERNATIVES {
                return Err(BraceLimit::Alternatives);
            }
            continue;
        };
        let (head, tail) = (&text[..group.open], &text[group.close + 1..]);
        for (start, end) in group.alternatives.into_iter().rev() {
            let alternative = &text[start..end];
            let mut expanded = String::with_capacity(head.len() + alternative.len() + tail.len());
            expanded.push_str(head);
            expanded.push_str(alternative);
            expanded.push_str(tail);
            pending.push((expanded, group.open));
        }
        if done.len() + pending.len() > MAX_ALTERNATIVES {
            return Err(BraceLimit::Alternatives);
        }
    }
    Ok(done)
}

/// A `{...}` group with at least one top-level comma.
struct Group {
    open: usize,
    close: usize,
    /// Byte ranges of the alternatives between `open` and `close`.
    alternatives: Vec<(usize, usize)>,
}

/// The first expandable group at or after byte `from`.
fn next_group(text: &str, from: usize, work: &mut usize) -> Result<Option<Group>, BraceLimit> {
    let bytes = text.as_bytes();
    let mut open = from;
    while open < bytes.len() {
        *work += 1;
        if *work > MAX_BRACE_WORK {
            return Err(BraceLimit::Work);
        }
        if bytes[open] != b'{' {
            open += 1;
            continue;
        }
        // Find the matching close, noting top-level commas.
        let mut depth = 1usize;
        let mut commas: Vec<usize> = Vec::new();
        let mut at = open + 1;
        let mut close = None;
        while at < bytes.len() {
            *work += 1;
            if *work > MAX_BRACE_WORK {
                return Err(BraceLimit::Work);
            }
            match bytes[at] {
                b'{' => {
                    depth += 1;
                    if depth > MAX_BRACE_DEPTH {
                        return Err(BraceLimit::Nesting);
                    }
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(at);
                        break;
                    }
                }
                b',' if depth == 1 => commas.push(at),
                _ => {}
            }
            at += 1;
        }
        let Some(close) = close else {
            // Unmatched: the rest has no complete group starting here.
            open += 1;
            continue;
        };
        if commas.is_empty() {
            // Literal braces; an inner group may still expand.
            open += 1;
            continue;
        }
        let mut alternatives = Vec::with_capacity(commas.len() + 1);
        let mut start = open + 1;
        for comma in commas {
            alternatives.push((start, comma));
            start = comma + 1;
        }
        alternatives.push((start, close));
        return Ok(Some(Group {
            open,
            close,
            alternatives,
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_matching() {
        assert!(matches("*", "web"));
        assert!(matches("app-*", "app-web"));
        assert!(!matches("app-*", "web-app"));
        assert!(matches("?eb", "web"));
        assert!(matches("a*b*c", "aXXbYYc"));
        assert!(!matches("a*b*c", "aXXbYY"));
    }

    fn expanded(pattern: &str) -> Vec<String> {
        braces(pattern).expect("within limits")
    }

    #[test]
    fn brace_alternatives() {
        assert_eq!(expanded("{apps,libs}/*"), vec!["apps/*", "libs/*"]);
        assert_eq!(expanded("packages/*"), vec!["packages/*"]);
        assert_eq!(
            expanded("{a,b}/{c,d}"),
            vec!["a/c", "a/d", "b/c", "b/d"],
            "left to right"
        );
        assert_eq!(expanded("x/{a,b}"), vec!["x/a", "x/b"]);
        assert_eq!(expanded("{a,}"), vec!["a", ""]);
    }

    #[test]
    fn nested_and_literal_braces() {
        assert_eq!(expanded("{a,{b,c}}"), vec!["a", "b", "c"]);
        assert_eq!(expanded("{a,b{c,d}}/x"), vec!["a/x", "bc/x", "bd/x"]);
        // No top-level comma: literal, but a group inside it still expands.
        assert_eq!(expanded("{}"), vec!["{}"]);
        assert_eq!(expanded("{a}"), vec!["{a}"]);
        assert_eq!(expanded("{{a,b}}"), vec!["{a}", "{b}"]);
        // Unmatched braces are literal.
        assert_eq!(expanded("{a,b"), vec!["{a,b"]);
        assert_eq!(expanded("a}"), vec!["a}"]);
        assert_eq!(expanded("{a,b}{"), vec!["a{", "b{"]);
    }

    #[test]
    fn brace_limits_yield_an_error_not_a_stack_overflow() {
        // The reported case: thousands of empty groups used to recurse per group.
        assert_eq!(braces(&"{}".repeat(8000)), Err(BraceLimit::Length));
        // Within the length cap, empty groups are cheap and literal.
        assert_eq!(expanded(&"{}".repeat(250)), vec!["{}".repeat(250)]);
        // Nesting depth.
        let deep = format!("{}a,b{}", "{".repeat(20), "}".repeat(20));
        assert_eq!(braces(&deep), Err(BraceLimit::Nesting));
        // Output size: 2^6 alternatives.
        assert_eq!(
            braces(&"{a,b}".repeat(6)),
            Err(BraceLimit::Alternatives),
            "64 alternatives exceed the limit of {MAX_ALTERNATIVES}"
        );
        assert!(braces(&"{a,b}".repeat(5)).is_ok());
        // Unclosed braces are scanned a bounded number of times.
        assert_eq!(
            braces(&"{".repeat(MAX_PATTERN_BYTES)),
            Err(BraceLimit::Nesting)
        );
        assert_eq!(
            expanded(&format!("{}x", "{a,".repeat(3))),
            vec!["{a,{a,{a,x"]
        );
    }
}
