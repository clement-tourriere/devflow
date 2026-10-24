//! The few things `faithful` needs from Git's configuration files, read from
//! the files themselves.
//!
//! libgit2 evaluates `gitdir` and `onbranch` includes, but it skips
//! `includeIf "hasconfig:remote.*.url:…"`, and a file it does not load is a
//! file whose `core.attributesFile` or partial-clone keys nobody has read. So
//! the include graph is walked here, in the order Git reads it (system, XDG,
//! global, the repository's `config` and `config.worktree`, each include
//! expanded in place), with every include resolved from the directory of the
//! file that holds it and every condition evaluated (`gitdir`, `gitdir/i`,
//! `onbranch`, `hasconfig`).
//!
//! Only three things are collected. `core.attributesFile` is a single setting,
//! so the last one read is the effective one, exactly as in Git: an earlier
//! value, even one that cannot be used, is overridden and decides nothing.
//! `extensions.partialClone` and `remote.*.promisor` are collected from the
//! repository's own files and what they include. Anything that cannot be
//! established, a file that exists and cannot be read, a line Git itself would
//! reject or a condition that cannot be evaluated, is recorded as `unknown`,
//! never as nothing.
use git2::Repository;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// Includes nested deeper than this (Git's own limit) are unknown.
const MAX_DEPTH: usize = 10;
/// Patterns and texts longer than this are not matched.
const MAX_GLOB: usize = 4096;

/// The process environment the walk depends on, injectable for tests.
#[derive(Debug, Clone, Default)]
pub(super) struct Env {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
}

impl Env {
    pub fn process() -> Env {
        Env {
            home: dirs::home_dir(),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        }
    }

    /// `$XDG_CONFIG_HOME`, else `~/.config`.
    fn xdg_dir(&self) -> Option<PathBuf> {
        self.xdg_config_home
            .clone()
            .or_else(|| self.home.as_ref().map(|h| h.join(".config")))
    }

    /// Where Git looks for the user's attributes when `core.attributesFile`
    /// is not set.
    pub fn default_attributes(&self) -> Option<PathBuf> {
        self.xdg_dir().map(|d| d.join("git").join("attributes"))
    }
}

/// What the effective `core.attributesFile` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AttributesSetting {
    /// An absolute path (`~/` expanded).
    Path(PathBuf),
    /// An empty value: Git then reads no user attributes at all, not even
    /// the default file.
    Disabled,
    /// A value that cannot be turned into a path here (relative to a
    /// directory only Git's process knows, another user's home, no value).
    Unusable(String),
}

#[derive(Debug, Default)]
pub(super) struct Findings {
    /// The last `core.attributesFile` Git would read; `None` when none is set.
    pub attributes_file: Option<AttributesSetting>,
    /// Lowercased keys and values (`remote.origin.promisor`) of the
    /// partial-clone keys found in the repository's own files.
    pub settings: Vec<(String, String)>,
    /// Why the walk is not complete.
    pub unknown: Option<String>,
}

/// One configuration file's root and whether it is the repository's own.
pub(super) struct Root {
    pub path: PathBuf,
    pub repository: bool,
}

/// The files Git reads for `repo`, in Git's order of precedence (a later file
/// overrides an earlier one).
pub(super) fn roots(repo: &Repository, env: &Env) -> Vec<Root> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let mut roots = Vec::new();
    if var("GIT_CONFIG_NOSYSTEM").is_none() {
        match var("GIT_CONFIG_SYSTEM") {
            Some(path) => roots.push((path.into(), false)),
            None => {
                for path in [
                    "/etc/gitconfig",
                    "/usr/local/etc/gitconfig",
                    "/opt/homebrew/etc/gitconfig",
                ] {
                    roots.push((path.into(), false));
                }
            }
        }
    }
    match var("GIT_CONFIG_GLOBAL") {
        Some(path) => roots.push((path.into(), false)),
        None => {
            if let Some(xdg) = env.xdg_dir() {
                roots.push((xdg.join("git").join("config"), false));
            }
            if let Some(home) = &env.home {
                roots.push((home.join(".gitconfig"), false));
            }
        }
    }
    roots.push((repo.commondir().join("config"), true));
    roots.push((repo.commondir().join("config.worktree"), true));
    if repo.path() != repo.commondir() {
        roots.push((repo.path().join("config.worktree"), true));
    }
    roots
        .into_iter()
        .map(|(path, repository)| Root { path, repository })
        .collect()
}

/// Walk the include graph from `roots`. `urls` are the configured remote URLs
/// (what `hasconfig:remote.*.url:` matches).
pub(super) fn walk(repo: &Repository, roots: &[Root], env: &Env, urls: &[String]) -> Findings {
    let mut git_dirs = vec![trim_slash(repo.path())];
    if let Ok(canonical) = repo.path().canonicalize() {
        git_dirs.push(trim_slash(&canonical));
    }
    git_dirs.dedup();
    let branch = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|r| r.symbolic_target().ok().flatten().map(str::to_string))
        .and_then(|t| t.strip_prefix("refs/heads/").map(str::to_string));
    let mut walker = Walker {
        git_dirs,
        branch,
        urls,
        home: env.home.as_deref(),
        out: Findings::default(),
    };
    for root in roots {
        walker.visit(&root.path, root.repository, 0);
    }
    walker.out
}

fn trim_slash(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.trim_end_matches('/').to_string()
}

struct Walker<'a> {
    git_dirs: Vec<String>,
    branch: Option<String>,
    urls: &'a [String],
    home: Option<&'a Path>,
    out: Findings,
}

impl Walker<'_> {
    fn unknown(&mut self, why: String) {
        self.out.unknown.get_or_insert(why);
    }

    fn visit(&mut self, path: &Path, repository: bool, depth: usize) {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            // Git ignores a file that is not there, and only that.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {
                self.unknown(format!(
                    "the configuration file {} exists and cannot be read",
                    path.display()
                ));
                return;
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let items = match parse(&text) {
            Ok(items) => items,
            Err(error) => {
                // Git would not read this file either: what it holds is not
                // "nothing".
                self.unknown(format!(
                    "bad config line {} in file {}",
                    error.line,
                    path.display()
                ));
                return;
            }
        };
        for item in items {
            let value = item.value.clone();
            match (
                item.section.as_str(),
                item.sub.as_deref(),
                item.key.as_str(),
            ) {
                ("core", None, "attributesfile") => {
                    // The last one read is the effective one.
                    self.out.attributes_file = Some(self.attributes_setting(value, path));
                }
                ("extensions", None, "partialclone") if repository => self
                    .out
                    .settings
                    .push(("extensions.partialclone".into(), value.unwrap_or_default())),
                ("remote", Some(name), "promisor") if repository => self.out.settings.push((
                    format!("remote.{}.promisor", name.to_ascii_lowercase()),
                    value.unwrap_or_else(|| "true".to_string()),
                )),
                ("include", None, "path") => {
                    if let Some(value) = &value {
                        self.follow(value, path, repository, depth);
                    }
                }
                ("includeif", Some(condition), "path") => {
                    let Some(value) = &value else { continue };
                    match self.holds(condition, path) {
                        Ok(true) => self.follow(value, path, repository, depth),
                        Ok(false) => {}
                        Err(why) => self.unknown(why),
                    }
                }
                _ => {}
            }
        }
    }

    /// Git's `git_config_pathname` for `core.attributesFile`: no value is an
    /// error, an empty one is no file, `~/` is the home directory. A relative
    /// path is relative to the directory Git ran in.
    fn attributes_setting(&self, value: Option<String>, _holder: &Path) -> AttributesSetting {
        let Some(value) = value else {
            return AttributesSetting::Unusable("core.attributesFile has no value".to_string());
        };
        if value.is_empty() {
            return AttributesSetting::Disabled;
        }
        if let Some(rest) = value.strip_prefix("~/") {
            return match self.home {
                Some(home) => AttributesSetting::Path(home.join(rest)),
                None => AttributesSetting::Unusable(
                    "core.attributesFile starts with ~/ and there is no home".to_string(),
                ),
            };
        }
        if value.starts_with('~') || value.starts_with("%(") {
            return AttributesSetting::Unusable(
                "core.attributesFile names another user's home or a Git prefix".to_string(),
            );
        }
        if value.starts_with('/') {
            return AttributesSetting::Path(PathBuf::from(value));
        }
        AttributesSetting::Unusable(
            "core.attributesFile is a relative path, which has no directory to be relative to"
                .to_string(),
        )
    }

    fn follow(&mut self, value: &str, from: &Path, repository: bool, depth: usize) {
        if depth + 1 > MAX_DEPTH {
            self.unknown("configuration includes nest too deeply".to_string());
            return;
        }
        match self.expand(value, from) {
            Ok(path) => self.visit(&path, repository, depth + 1),
            Err(why) => self.unknown(why),
        }
    }

    /// `~/` is the home directory, a relative path is relative to the
    /// directory of the file that holds it.
    fn expand(&self, value: &str, holder: &Path) -> Result<PathBuf, String> {
        if let Some(rest) = value.strip_prefix("~/") {
            return match self.home {
                Some(home) => Ok(home.join(rest)),
                None => Err("a configuration path starts with ~/ and there is no home".into()),
            };
        }
        if value.starts_with('~') {
            return Err("a configuration path names another user's home".into());
        }
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Ok(path);
        }
        Ok(holder.parent().unwrap_or(Path::new("")).join(path))
    }

    /// Whether an `includeIf` condition holds; `Err` when it cannot be told.
    fn holds(&self, condition: &str, holder: &Path) -> Result<bool, String> {
        if let Some(pattern) = condition.strip_prefix("gitdir/i:") {
            return self.gitdir(pattern, true, holder);
        }
        if let Some(pattern) = condition.strip_prefix("gitdir:") {
            return self.gitdir(pattern, false, holder);
        }
        if let Some(pattern) = condition.strip_prefix("onbranch:") {
            let Some(branch) = &self.branch else {
                return Ok(false);
            };
            let mut pattern = pattern.to_string();
            if pattern.ends_with('/') {
                pattern.push_str("**");
            }
            return matches(&pattern, branch, false);
        }
        if let Some(pattern) = condition.strip_prefix("hasconfig:remote.*.url:") {
            for url in self.urls {
                if matches(pattern, url, false)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        if condition.starts_with("hasconfig:") {
            return Err(
                "an includeIf hasconfig condition other than remote.*.url cannot be evaluated"
                    .to_string(),
            );
        }
        // Git ignores a condition it does not know.
        Ok(false)
    }

    fn gitdir(&self, pattern: &str, icase: bool, holder: &Path) -> Result<bool, String> {
        let mut pattern = if let Some(rest) = pattern.strip_prefix("~/") {
            match self.home {
                Some(home) => format!("{}/{rest}", trim_slash(home)),
                None => {
                    return Err("a gitdir condition starts with ~/ and there is no home".into());
                }
            }
        } else if let Some(rest) = pattern.strip_prefix("./") {
            let dir = holder.parent().unwrap_or(Path::new(""));
            format!("{}/{rest}", trim_slash(dir))
        } else if pattern.starts_with('/') {
            pattern.to_string()
        } else {
            format!("**/{pattern}")
        };
        if pattern.ends_with('/') {
            pattern.push_str("**");
        }
        for dir in &self.git_dirs {
            if matches(&pattern, dir, icase)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// One `key = value` line with the section it is in.
#[derive(Debug, PartialEq, Eq)]
struct Item {
    /// Lowercased.
    section: String,
    sub: Option<String>,
    /// Lowercased.
    key: String,
    /// `None` for a key without a value (a boolean `true`).
    value: Option<String>,
}

/// A line Git would reject ("bad config line N").
#[derive(Debug, Default, PartialEq, Eq)]
struct ParseError {
    line: usize,
}

/// Git's configuration syntax, read the way `git_parse_source` reads it: a
/// leading UTF-8 byte order mark is skipped, `\r\n` is a newline, sections are
/// `[a]`, `[a "b"]` or `[a.b]` (the last lowercased throughout), keys are
/// letters, digits and `-` and start with a letter, values group with quotes,
/// escape only `\n`, `\t`, `\b`, `\\`, `\"` and a continued line, and `#` or
/// `;` outside quotes starts a comment. What Git refuses, this refuses: an
/// error carries the line it was found on.
fn parse(text: &str) -> Result<Vec<Item>, ParseError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut input = Input::new(text);
    let mut items = Vec::new();
    // `section.sub` as Git builds the variable name stem, and the split of it.
    let mut stem: Option<(String, Option<String>)> = None;
    let mut comment = false;
    loop {
        let c = input.next();
        if c == '\n' {
            if input.eof {
                return Ok(items);
            }
            comment = false;
            continue;
        }
        if comment || space(c) {
            continue;
        }
        if c == '#' || c == ';' {
            comment = true;
            continue;
        }
        if c == '[' {
            stem = Some(header(&mut input).ok_or(ParseError { line: input.line })?);
            continue;
        }
        if !c.is_ascii_alphabetic() {
            return Err(ParseError { line: input.line });
        }
        // A key before any section is accepted, with no section to its name.
        let (section, sub) = stem.clone().unwrap_or_default();
        let mut key = c.to_ascii_lowercase().to_string();
        let mut after;
        loop {
            after = input.next();
            if input.eof || !(after.is_ascii_alphanumeric() || after == '-') {
                break;
            }
            key.push(after.to_ascii_lowercase());
        }
        while after == ' ' || after == '\t' {
            after = input.next();
        }
        let value = if after == '\n' {
            // The newline ends the line: re-examine it as the loop's own, so
            // an end of file is seen.
            input.unread(after);
            None
        } else if after == '=' {
            Some(value(&mut input).ok_or(ParseError { line: input.line })?)
        } else {
            return Err(ParseError { line: input.line });
        };
        items.push(Item {
            section,
            sub,
            key,
            value,
        });
    }
}

/// The characters of a file, with Git's treatment of line ends: `\r\n` reads
/// as `\n`, and the end of the file reads as one final `\n`.
struct Input {
    chars: Vec<char>,
    at: usize,
    line: usize,
    eof: bool,
    pushed_back: Option<char>,
}

impl Input {
    fn new(text: &str) -> Input {
        Input {
            chars: text.chars().collect(),
            at: 0,
            line: 1,
            eof: false,
            pushed_back: None,
        }
    }

    fn next(&mut self) -> char {
        if let Some(c) = self.pushed_back.take() {
            if c == '\n' {
                self.line += 1;
            }
            return c;
        }
        let Some(&c) = self.chars.get(self.at) else {
            self.eof = true;
            self.line += 1;
            return '\n';
        };
        self.at += 1;
        let c = if c == '\r' && self.chars.get(self.at) == Some(&'\n') {
            self.at += 1;
            '\n'
        } else {
            c
        };
        if c == '\n' {
            self.line += 1;
        }
        c
    }

    fn unread(&mut self, c: char) {
        self.pushed_back = Some(c);
        if c == '\n' {
            self.line -= 1;
        }
    }
}

/// Git's `isspace`: blank, tab, newline, carriage return.
fn space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// After `[`: `name]`, `name "sub"]` or the old `name.sub]` (lowercased).
fn header(input: &mut Input) -> Option<(String, Option<String>)> {
    let mut name = String::new();
    loop {
        let c = input.next();
        if input.eof {
            return None;
        }
        if c == ']' {
            break;
        }
        if space(c) {
            return extended_header(input, name, c);
        }
        if !key_char(c) && c != '.' {
            return None;
        }
        name.push(c.to_ascii_lowercase());
    }
    if name.is_empty() {
        return None;
    }
    Some(match name.split_once('.') {
        // The old form: the subsection is lowercase too.
        Some((section, sub)) => (section.to_string(), Some(sub.to_string())),
        None => (name, None),
    })
}

fn extended_header(
    input: &mut Input,
    name: String,
    mut c: char,
) -> Option<(String, Option<String>)> {
    loop {
        if c == '\n' {
            input.line -= 1;
            return None;
        }
        c = input.next();
        if !space(c) {
            break;
        }
    }
    // `[base "extension"]`
    if c != '"' {
        return None;
    }
    let mut sub = String::new();
    loop {
        let c = input.next();
        if c == '\n' {
            input.line -= 1;
            return None;
        }
        if c == '"' {
            break;
        }
        if c == '\\' {
            let escaped = input.next();
            if escaped == '\n' {
                input.line -= 1;
                return None;
            }
            sub.push(escaped);
            continue;
        }
        sub.push(c);
    }
    if input.next() != ']' {
        return None;
    }
    if name.is_empty() {
        return None;
    }
    Some(match name.split_once('.') {
        Some((section, rest)) => (section.to_string(), Some(format!("{rest}.{sub}"))),
        None => (name, Some(sub)),
    })
}

/// After `=`: the value, up to the end of the line. `None` for what Git
/// rejects (an unknown escape, an unterminated quote).
fn value(input: &mut Input) -> Option<String> {
    let mut out = String::new();
    let (mut quote, mut comment, mut pending) = (false, false, 0usize);
    loop {
        let c = input.next();
        if c == '\n' {
            if quote {
                input.line -= 1;
                return None;
            }
            return Some(out);
        }
        if comment {
            continue;
        }
        if space(c) && !quote {
            if !out.is_empty() {
                pending += 1;
            }
            continue;
        }
        if !quote && (c == ';' || c == '#') {
            comment = true;
            continue;
        }
        out.extend(std::iter::repeat_n(' ', pending));
        pending = 0;
        if c == '\\' {
            let escaped = match input.next() {
                '\n' => continue,
                't' => '\t',
                'b' => '\u{8}',
                'n' => '\n',
                e @ ('\\' | '"') => e,
                _ => return None,
            };
            out.push(escaped);
            continue;
        }
        if c == '"' {
            quote = !quote;
            continue;
        }
        out.push(c);
    }
}

/// One member of a bracket expression.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Member {
    Char(char),
    Range(char, char),
    Posix(Posix),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Posix {
    Alnum,
    Alpha,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
}

impl Posix {
    fn named(name: &str) -> Option<Posix> {
        Some(match name {
            "alnum" => Posix::Alnum,
            "alpha" => Posix::Alpha,
            "blank" => Posix::Blank,
            "cntrl" => Posix::Cntrl,
            "digit" => Posix::Digit,
            "graph" => Posix::Graph,
            "lower" => Posix::Lower,
            "print" => Posix::Print,
            "punct" => Posix::Punct,
            "space" => Posix::Space,
            "upper" => Posix::Upper,
            "xdigit" => Posix::Xdigit,
            _ => return None,
        })
    }

    /// With `fold`, Git counts a letter of either case for `lower` and `upper`.
    fn holds(self, t: char, fold: bool) -> bool {
        match self {
            Posix::Alnum => t.is_ascii_alphanumeric(),
            Posix::Alpha => t.is_ascii_alphabetic(),
            Posix::Blank => t == ' ' || t == '\t',
            Posix::Cntrl => t.is_ascii_control(),
            Posix::Digit => t.is_ascii_digit(),
            Posix::Graph => t.is_ascii_graphic(),
            Posix::Lower => t.is_ascii_lowercase() || (fold && t.is_ascii_uppercase()),
            Posix::Print => t.is_ascii_graphic() || t == ' ',
            Posix::Punct => t.is_ascii_punctuation(),
            Posix::Space => t.is_ascii_whitespace() || t == '\u{b}',
            Posix::Upper => t.is_ascii_uppercase() || (fold && t.is_ascii_lowercase()),
            Posix::Xdigit => t.is_ascii_hexdigit(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(char),
    /// `?`: one character but `/`.
    One,
    /// `*`: any run of characters but `/`.
    Star,
    /// `**/` at the start or after a `/`: zero or more directories.
    Dirs,
    /// `**` where it may cross `/` (at the end after a `/`, or before `\/`).
    Rest,
    /// `[…]`: one character but `/`.
    Class {
        negated: bool,
        members: Vec<Member>,
    },
    /// A malformed bracket expression: Git gives up on the whole match.
    Abort,
}

/// Parse the bracket expression at `chars[at]` (the `[`) the way Git's
/// `dowild` reads it; returns the token and the index of its closing `]`.
fn bracket(chars: &[char], at: usize) -> (Token, usize) {
    let mut i = at + 1;
    let mut current = chars.get(i).copied();
    if current == Some('^') {
        current = Some('!');
    }
    let negated = current == Some('!');
    if negated {
        i += 1;
        current = chars.get(i).copied();
    }
    let mut members = Vec::new();
    let mut previous: Option<char> = None;
    loop {
        let Some(c) = current else {
            return (Token::Abort, i);
        };
        // What the next member sees as its predecessor.
        let mut remembered = Some(c);
        if c == '\\' {
            i += 1;
            let Some(&escaped) = chars.get(i) else {
                return (Token::Abort, i);
            };
            members.push(Member::Char(escaped));
            remembered = Some(escaped);
        } else if c == '-' && previous.is_some() && chars.get(i + 1).is_some_and(|n| *n != ']') {
            i += 1;
            let mut high = chars[i];
            if high == '\\' {
                i += 1;
                let Some(&escaped) = chars.get(i) else {
                    return (Token::Abort, i);
                };
                high = escaped;
            }
            members.push(Member::Range(previous.unwrap_or('-'), high));
            remembered = None;
        } else if c == '[' && chars.get(i + 1) == Some(&':') {
            let start = i + 2;
            let mut end = start;
            while end < chars.len() && chars[end] != ']' {
                end += 1;
            }
            if end >= chars.len() {
                return (Token::Abort, end);
            }
            if end == start || chars[end - 1] != ':' {
                // No `:]`: an ordinary `[`.
                members.push(Member::Char('['));
                remembered = Some('[');
            } else {
                let name: String = chars[start..end - 1].iter().collect();
                let Some(class) = Posix::named(&name) else {
                    return (Token::Abort, end);
                };
                members.push(Member::Posix(class));
                remembered = None;
                i = end;
            }
        } else {
            members.push(Member::Char(c));
        }
        previous = remembered;
        i += 1;
        current = chars.get(i).copied();
        if current == Some(']') {
            return (Token::Class { negated, members }, i);
        }
    }
}

fn compile(pattern: &str, fold: bool) -> Vec<Token> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '[' => {
                let (token, close) = bracket(&chars, i);
                let abort = token == Token::Abort;
                tokens.push(token);
                if abort {
                    return tokens;
                }
                i = close + 1;
            }
            '\\' => {
                // An escaped character is compared as it is.
                tokens.push(Token::Literal(chars.get(i + 1).copied().unwrap_or('\\')));
                i += 2;
            }
            '?' => {
                tokens.push(Token::One);
                i += 1;
            }
            '*' => {
                let mut stars = 0;
                while chars.get(i + stars) == Some(&'*') {
                    stars += 1;
                }
                let boundary_before = i == 0 || chars[i - 1] == '/';
                let next = chars.get(i + stars);
                let escaped_slash_next =
                    next == Some(&'\\') && chars.get(i + stars + 1) == Some(&'/');
                if stars >= 2 && boundary_before && next == Some(&'/') {
                    tokens.push(Token::Dirs);
                    i += stars + 1;
                } else if stars >= 2 && boundary_before && (next.is_none() || escaped_slash_next) {
                    tokens.push(Token::Rest);
                    i += stars;
                } else {
                    tokens.push(Token::Star);
                    i += stars;
                }
            }
            c => {
                tokens.push(Token::Literal(if fold {
                    c.to_ascii_lowercase()
                } else {
                    c
                }));
                i += 1;
            }
        }
    }
    tokens
}

fn class_holds(negated: bool, members: &[Member], t: char, fold: bool) -> bool {
    if t == '/' {
        return false;
    }
    let found = members.iter().any(|m| match m {
        Member::Char(c) => t == *c,
        Member::Range(low, high) => {
            (t >= *low && t <= *high)
                || (fold && t.is_ascii_lowercase() && {
                    let upper = t.to_ascii_uppercase();
                    upper >= *low && upper <= *high
                })
        }
        Member::Posix(class) => class.holds(t, fold),
    });
    found != negated
}

/// Git's `wildmatch` with `WM_PATHNAME` (and `WM_CASEFOLD` when `icase`):
/// `*`, `?` and `[…]` stop at `/`, `**/` matches zero or more directories and
/// a trailing `/**` everything below. Bracket expressions take ranges,
/// `[!…]` and `[^…]` negation, `[[:alpha:]]`-style classes and backslash
/// escapes. The cost is the pattern's length times the text's, whatever the
/// pattern.
fn matches(pattern: &str, text: &str, icase: bool) -> Result<bool, String> {
    if pattern.len() > MAX_GLOB || text.len() > MAX_GLOB {
        return Err("a pattern or the text it is matched against is too long".to_string());
    }
    let tokens = compile(pattern, icase);
    let text: Vec<char> = text
        .chars()
        .map(|c| if icase { c.to_ascii_lowercase() } else { c })
        .collect();
    let n = text.len();
    // `next[i]`: the tokens after this one match text[i..].
    let mut next: Vec<bool> = (0..=n).map(|i| i == n).collect();
    for token in tokens.iter().rev() {
        let mut current = vec![false; n + 1];
        // For `Dirs`: inside a directory name, which must end in `/`.
        let mut inside = vec![false; n + 1];
        for i in (0..=n).rev() {
            current[i] = match token {
                Token::Literal(c) => i < n && text[i] == *c && next[i + 1],
                Token::One => i < n && text[i] != '/' && next[i + 1],
                Token::Star => next[i] || (i < n && text[i] != '/' && current[i + 1]),
                Token::Rest => next[i] || (i < n && current[i + 1]),
                Token::Class { negated, members } => {
                    i < n && class_holds(*negated, members, text[i], icase) && next[i + 1]
                }
                Token::Abort => false,
                Token::Dirs => {
                    inside[i] = i < n
                        && if text[i] == '/' {
                            current[i + 1]
                        } else {
                            inside[i + 1]
                        };
                    next[i] || inside[i]
                }
            };
        }
        next = current;
    }
    Ok(next[0])
}
