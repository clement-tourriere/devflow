//! Documentation links. Every relative link in a Markdown file points at a
//! file that exists and, when it names a heading, at a heading that exists
//! (GitHub's spelling of it); and every link into this repository that the
//! code, the scripts and the examples carry (`…/blob/main/PATH#anchor`, the
//! raw schema URLs) does too, so moving a document or renaming a heading
//! fails here instead of in a person's browser.
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};

const REPO_URLS: [&str; 2] = [
    "https://github.com/clement-tourriere/devflow/blob/main/",
    "https://raw.githubusercontent.com/clement-tourriere/devflow/main/",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Files under `dir` that `keep` accepts. Build output, `.git` and hidden
/// directories (other than `.github`) are not looked into, and links are not
/// followed.
fn walk(dir: &Path, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap())
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry.file_type().unwrap();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if name == "target"
                || name == "node_modules"
                || (name.starts_with('.') && name != ".github")
            {
                continue;
            }
            walk(&path, keep, out);
        } else if keep(&path) {
            out.push(path);
        }
    }
}

fn files_with(extensions: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(
        &root(),
        &|path| {
            path.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| extensions.contains(&e))
        },
        &mut out,
    );
    out
}

/// A line with its inline code spans blanked out, so what they quote is not
/// read as a link.
fn without_code_spans(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i] == '`' {
            i += 1;
        }
        let ticks = i - start;
        // The span ends at the next run of exactly as many backticks.
        let mut j = i;
        let mut end = None;
        while j < chars.len() {
            if chars[j] == '`' {
                let run_start = j;
                while j < chars.len() && chars[j] == '`' {
                    j += 1;
                }
                if j - run_start == ticks {
                    end = Some(j);
                    break;
                }
            } else {
                j += 1;
            }
        }
        match end {
            Some(end) => {
                out.push(' ');
                i = end;
            }
            None => out.extend(std::iter::repeat_n('`', ticks)),
        }
    }
    out
}

/// A heading's text as the reader sees it: no code ticks, emphasis or link
/// targets.
fn plain(heading: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = heading.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' | '*' => i += 1,
            '[' => i += 1,
            ']' if chars.get(i + 1) == Some(&'(') => {
                while i < chars.len() && chars[i] != ')' {
                    i += 1;
                }
                i += 1;
            }
            '<' => {
                while i < chars.len() && chars[i] != '>' {
                    i += 1;
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// GitHub's anchor for a heading: lowercase, spaces become hyphens, anything
/// else that is not a letter, a digit, `-` or `_` is dropped.
fn slug(heading: &str) -> String {
    plain(heading)
        .trim()
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// The lines of a Markdown file outside fenced code blocks, with numbers.
fn prose(text: &str) -> Vec<(usize, &str)> {
    let mut fence: Option<(char, usize)> = None;
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let marker = trimmed
            .chars()
            .next()
            .filter(|c| *c == '`' || *c == '~')
            .map(|c| (c, trimmed.chars().take_while(|x| *x == c).count()))
            .filter(|(_, n)| *n >= 3);
        match (fence, marker) {
            (None, Some(m)) => fence = Some(m),
            (Some((c, n)), Some((mc, mn)))
                if c == mc && mn >= n && trimmed.trim_end_matches(c).trim().is_empty() =>
            {
                fence = None
            }
            (Some(_), _) => {}
            (None, None) => out.push((index + 1, line)),
        }
    }
    out
}

/// The anchors a Markdown file offers: its headings (repeats get `-1`, `-2`)
/// and explicit `id`/`name` attributes.
fn anchors(text: &str) -> BTreeSet<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out = BTreeSet::new();
    for (_, line) in prose(text) {
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes)
            && line.chars().nth(hashes) == Some(' ')
            && line.len() > hashes + 1
        {
            let title = line[hashes + 1..].trim().trim_end_matches('#').trim_end();
            let base = slug(title);
            let n = seen.entry(base.clone()).or_insert(0);
            out.insert(if *n == 0 {
                base.clone()
            } else {
                format!("{base}-{n}")
            });
            *n += 1;
        }
        for key in ["id=\"", "name=\""] {
            let mut rest = line;
            while let Some(at) = rest.find(key) {
                rest = &rest[at + key.len()..];
                if let Some(end) = rest.find('"') {
                    out.insert(rest[..end].to_string());
                }
            }
        }
    }
    out
}

/// Every link target in a Markdown file, with its line: inline links,
/// reference definitions, and `href`/`src` attributes of inline HTML.
fn targets(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (number, line) in prose(text) {
        let line = without_code_spans(line);
        let mut rest = line.as_str();
        while let Some(at) = rest.find("](") {
            let after = &rest[at + 2..];
            let mut depth = 1;
            let mut end = after.len();
            for (i, c) in after.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let inner = after[..end].trim();
            let target = if let Some(stripped) = inner.strip_prefix('<') {
                stripped.split('>').next().unwrap_or("")
            } else {
                inner.split_whitespace().next().unwrap_or("")
            };
            out.push((number, target.to_string()));
            rest = &after[end.min(after.len())..];
        }
        if let Some(def) = line.trim_start().strip_prefix('[')
            && let Some((_, tail)) = def.split_once("]:")
            && let Some(target) = tail.split_whitespace().next()
        {
            out.push((number, target.to_string()));
        }
        for key in ["href=\"", "src=\""] {
            let mut rest = line.as_str();
            while let Some(at) = rest.find(key) {
                rest = &rest[at + key.len()..];
                if let Some(end) = rest.find('"') {
                    out.push((number, rest[..end].to_string()));
                }
            }
        }
    }
    out
}

struct Checker {
    anchors: HashMap<PathBuf, BTreeSet<String>>,
    broken: Vec<String>,
}

impl Checker {
    fn anchors_of(&mut self, file: &Path) -> &BTreeSet<String> {
        self.anchors.entry(file.to_path_buf()).or_insert_with(|| {
            anchors(&fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display())))
        })
    }

    /// Check a target in the repository: `path` is relative to `base`, or to
    /// the repository root when it starts with `/`.
    fn check(&mut self, origin: &str, base: &Path, target: &str) {
        let (path, anchor) = match target.split_once('#') {
            Some((p, a)) => (p, a),
            None => (target, ""),
        };
        let path = path.split('?').next().unwrap_or("");
        let resolved = if path.is_empty() {
            base.to_path_buf()
        } else if let Some(absolute) = path.strip_prefix('/') {
            root().join(absolute)
        } else {
            base.parent().unwrap().join(path)
        };
        // Lexical normalisation, so `docs/../README.md` reads as `README.md`.
        let mut normal = PathBuf::new();
        for component in resolved.components() {
            match component {
                Component::ParentDir => {
                    normal.pop();
                }
                Component::CurDir => {}
                other => normal.push(other),
            }
        }
        if !normal.exists() {
            self.broken
                .push(format!("{origin}: {target}: no such file"));
            return;
        }
        if anchor.is_empty() || normal.extension().and_then(|e| e.to_str()) != Some("md") {
            return;
        }
        let wanted = anchor.to_lowercase();
        if !self.anchors_of(&normal).contains(&wanted) {
            self.broken.push(format!(
                "{origin}: {target}: no heading `#{anchor}` in {}",
                normal.display()
            ));
        }
    }
}

fn relative(path: &Path) -> String {
    path.strip_prefix(root())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[test]
fn relative_links_in_markdown_resolve() {
    let mut checker = Checker {
        anchors: HashMap::new(),
        broken: Vec::new(),
    };
    let documents = files_with(&["md"]);
    assert!(
        documents.len() > 10,
        "found only {} Markdown files",
        documents.len()
    );
    for file in documents {
        let text = fs::read_to_string(&file).unwrap();
        for (line, target) in targets(&text) {
            let skip = target.is_empty()
                || target.contains("://")
                || target.starts_with("mailto:")
                || target.starts_with("tel:");
            if skip {
                continue;
            }
            checker.check(&format!("{}:{line}", relative(&file)), &file, &target);
        }
    }
    assert!(
        checker.broken.is_empty(),
        "{} broken documentation link(s):\n{}",
        checker.broken.len(),
        checker.broken.join("\n")
    );
}

/// The code, the scripts and the examples link to this repository's files by
/// their GitHub address (the migration guide, the guide for a checkout devflow
/// does not make, the schemas): those pages and headings must exist too.
#[test]
fn links_into_this_repository_resolve() {
    let mut checker = Checker {
        anchors: HashMap::new(),
        broken: Vec::new(),
    };
    let mut seen = 0;
    for file in files_with(&["rs", "sh", "toml", "md", "json", "yml", "yaml", "pkl", "py"]) {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        if relative(&file) == "tests/docs.rs" {
            continue;
        }
        for (index, line) in text.lines().enumerate() {
            for prefix in REPO_URLS {
                let mut rest = line;
                while let Some(at) = rest.find(prefix) {
                    rest = &rest[at + prefix.len()..];
                    let end = rest
                        .find(|c: char| c.is_whitespace() || "\"'`)>],\\".contains(c))
                        .unwrap_or(rest.len());
                    let target = rest[..end].trim_end_matches(['.', ';']);
                    if target.is_empty() {
                        continue;
                    }
                    seen += 1;
                    let origin = format!("{}:{}", relative(&file), index + 1);
                    checker.check(&origin, &root().join("x"), target);
                }
            }
        }
    }
    assert!(
        seen >= 4,
        "expected links into the repository, found {seen}"
    );
    assert!(
        checker.broken.is_empty(),
        "{} broken link(s) into the repository:\n{}",
        checker.broken.len(),
        checker.broken.join("\n")
    );
}

#[test]
fn heading_anchors_follow_githubs_spelling() {
    let text = "# Title\n\n## Environment (`[env]`)\n\n## A, b: c\n\n```sh\n# not a heading\n```\n\n## Repeat\n\n## Repeat\n\n<a id=\"named\"></a>\n";
    let found = anchors(text);
    for expected in [
        "title",
        "environment-env",
        "a-b-c",
        "repeat",
        "repeat-1",
        "named",
    ] {
        assert!(found.contains(expected), "{expected}: {found:?}");
    }
    assert!(!found.contains("not-a-heading"));
    assert_eq!(
        slug("`devflow init`: a proposed setup"),
        "devflow-init-a-proposed-setup"
    );
    assert_eq!(
        slug("Seed data for automation (`[data]`)"),
        "seed-data-for-automation-data"
    );
}

#[test]
fn links_are_read_outside_code() {
    let text = "[a](x.md#h) and `[b](y.md)` and [`c`](z.md \"title\")\n\n```\n[d](w.md)\n```\n<img src=\"i.png\" />\n";
    let found: Vec<String> = targets(text).into_iter().map(|(_, t)| t).collect();
    assert_eq!(found, ["x.md#h", "z.md", "i.png"]);
}
