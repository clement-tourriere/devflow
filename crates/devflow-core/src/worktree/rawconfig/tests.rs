//! The reader and the matcher, judged against the `git` on `PATH`.
use super::*;
use crate::worktree::gitoracle::{available, git, stdout};

fn m(pattern: &str, text: &str) -> bool {
    matches(pattern, text, false).unwrap()
}

/// How `git config --list -z` prints an item: `name\nvalue`, or `name`.
fn listed(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|i| {
            let name = match (&i.sub, i.section.as_str()) {
                (_, "") => i.key.clone(),
                (Some(sub), _) => format!("{}.{}.{}", i.section, sub, i.key),
                (None, _) => format!("{}.{}", i.section, i.key),
            };
            match &i.value {
                Some(value) => format!("{name}\n{value}"),
                None => name,
            }
        })
        .collect()
}

#[test]
fn the_syntax_is_read_like_git_reads_it() {
    let items = parse(
        "# comment\n[fetch]\n\tprune\n[core]\n\tattributesFile = \"~/a b\" ; trailing\n\
         [Remote \"Origin\"] promisor = 2\n[includeIf \"gitdir:~/work/\"]\n\tpath = x\\\n.inc\n\
         [old.Sub]\n\tkey = v\n",
    )
    .unwrap();
    assert_eq!(
        listed(&items),
        [
            "fetch.prune",
            "core.attributesfile\n~/a b",
            "remote.Origin.promisor\n2",
            "includeif.gitdir:~/work/.path\nx.inc",
            "old.sub.key\nv",
        ]
    );
}

#[test]
fn a_leading_byte_order_mark_is_skipped() {
    let items = parse("\u{feff}[core]\n\tattributesFile = /dev/null\n").unwrap();
    assert_eq!(listed(&items), ["core.attributesfile\n/dev/null"]);
    // Only at the very start.
    assert!(parse("[core]\n\u{feff}[x]\n").is_err());
}

/// Texts of configuration files, valid and not. `git config --file F --list`
/// and [`parse`] must agree on whether the file is acceptable and, when it is,
/// on every name and value.
const SYNTAX: &[&str] = &[
    "[core]\n\tbare = false\n",
    "\u{feff}[core]\n\tx = 1\n",
    "[a]\nb\n",
    "[a]\nb",
    "[a]\nb = 1",
    "[a \"B c\"]\n k = v \\\n  w\n",
    "[a.B]\nk=\"q  q\"  # c\n",
    "[a]\nk = \\x\n",
    "[a]\nk = \"open\n",
    "[a\n",
    "[]\n",
    "k = 1\n",
    "[a]\n1k = x\n",
    "[a]\nk x = 1\n",
    "[a]\r\nk = v\r\n",
    "[a] k = v\n",
    "[a]\nk = v ; c\nm=1\n",
    "[a]\n K-1 = Y\n",
    "[a ]\nk = 1\n",
    "[a \"x\\\"y\"]\nk=1\n",
    "[a \"x\\\\y\"]\nk=1\n",
    "[a]\nk = \t  v  w \t \n",
    "[a]\nk = \"  lead\"\n",
    "[a]\nk = a\\\nb\n",
    "[a]\nk = \"\\\"q\\\"\"\n",
    "[x]\n; comment\n# c\n  [y]\n z = 1\n",
    "[a]\n\tk\n\t= 1\n",
    "[a]\nk = \\n\\t\\b\n",
    "[a \"b\"] [c]\nk = 1\n",
    "[a]\nk = 1 # c \\\nstill comment\nm = 2\n",
    "[a]\nk = \"x\"y\"z\"\n",
    "[Section.Sub \"Case\"]\nK = V\n",
    "[a \"unterminated\n",
    "[a \"b\" x]\nk=1\n",
    "[a]\n=\n",
    "[a]\nk=\nm=2\n",
    "[a]\n  # c\nk = 1\n\n\n",
    "[a_b]\nk = 1\n",
    "[a]\nk_x = 1\n",
    "[a]\nk = # c\n",
    "[a]\nk =;x\n",
    "[a1]\nk = 1\n",
    "[a-b]\nk = 1\n",
    "[a]\nk = h\u{e9}llo\n",
    "[a]\nk = \"a\\\nb\"\n",
    "\u{feff}\u{feff}[a]\nk = 1\n",
    "[a]\nk = 1\n\u{feff}",
    "[a \"b\"\n]\nk = 1\n",
    "[a]\nk = \"1\n2\"\n",
];

#[test]
fn the_syntax_agrees_with_git() {
    if !available() {
        eprintln!("git is not installed: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (n, text) in SYNTAX.iter().enumerate() {
        let file = dir.path().join(format!("c{n}"));
        std::fs::write(&file, text).unwrap();
        let out = git(
            dir.path(),
            dir.path(),
            &["config", "--file", file.to_str().unwrap(), "--list", "-z"],
        );
        let mine = parse(text);
        if out.status.success() {
            let git_lines: Vec<String> = stdout(&out)
                .split('\0')
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            let items = mine.unwrap_or_else(|e| panic!("git accepts {text:?}, line {}", e.line));
            assert_eq!(listed(&items), git_lines, "{text:?}");
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            let error = mine.expect_err(&format!("git refuses {text:?} ({})", stderr.trim()));
            // The same line, when Git says which.
            if let Some(line) = stderr
                .split("bad config line ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse::<usize>().ok())
            {
                assert_eq!(error.line, line, "{text:?}: {stderr}");
            }
        }
    }
}

#[test]
fn an_error_names_the_line() {
    assert_eq!(parse("[a]\nk = 1\n[b\n").unwrap_err().line, 3);
    assert_eq!(parse("[a]\nk = \\q\n").unwrap_err().line, 2);
}

/// `(expected, pattern, text)`, modelled on Git's t3070-wildmatch (the
/// `pathmatch` column: `WM_PATHNAME`). Git decides; the expected value is what
/// that test says, and the three have to agree.
const WILDMATCH: &[(bool, &str, &str)] = &[
    (true, "foo", "foo"),
    (false, "bar", "foo"),
    (true, "???", "foo"),
    (false, "??", "foo"),
    (true, "*", "foo"),
    (true, "f*", "foo"),
    (false, "*f", "foo"),
    (true, "*foo*", "foo"),
    (true, "*ob*a*r*", "foobar"),
    (true, "*ab", "aaaaaaabababab"),
    (true, r"foo\*", "foo*"),
    (false, r"foo\*bar", "foobar"),
    (true, r"f\\oo", r"f\oo"),
    (true, "*[al]?", "ball"),
    (false, "[ten]", "ten"),
    (true, "**[!te]", "ten"),
    (false, "**[!ten]", "ten"),
    (true, "t[a-g]n", "ten"),
    (false, "t[!a-g]n", "ten"),
    (true, "t[!a-g]n", "ton"),
    (true, "t[^a-g]n", "ton"),
    (true, "a[]]b", "a]b"),
    (true, "a[]-]b", "a-b"),
    (true, "a[]-]b", "a]b"),
    (false, "a[]-]b", "aab"),
    (true, "a[]a-]b", "aab"),
    (true, "]", "]"),
    (false, "foo*bar", "foo/baz/bar"),
    (false, "foo**bar", "foo/baz/bar"),
    (true, "foo**bar", "foobazbar"),
    (true, "foo/**/bar", "foo/baz/bar"),
    (true, "foo/**/**/bar", "foo/baz/bar"),
    (true, "foo/**/bar", "foo/b/a/z/bar"),
    (true, "foo/**/**/bar", "foo/b/a/z/bar"),
    (true, "foo/**/bar", "foo/bar"),
    (true, "foo/**/**/bar", "foo/bar"),
    (false, "foo?bar", "foo/bar"),
    (false, "foo[/]bar", "foo/bar"),
    (false, "foo[^a-z]bar", "foo/bar"),
    (false, "f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo/bar"),
    (true, "f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo-bar"),
    (true, "**/foo", "foo"),
    (true, "**/foo", "XXX/foo"),
    (true, "**/foo", "bar/baz/foo"),
    (false, "*/foo", "bar/baz/foo"),
    (false, "**/bar*", "foo/bar/baz"),
    (true, "**/bar/*", "deep/foo/bar/baz"),
    (false, "**/bar/*", "deep/foo/bar/baz/"),
    (true, "**/bar/**", "deep/foo/bar/baz/"),
    (false, "**/bar/*", "deep/foo/bar"),
    (true, "**/bar/**", "deep/foo/bar/"),
    (false, "**/bar**", "foo/bar/baz"),
    (true, "*/bar/**", "foo/bar/baz/x"),
    (false, "*/bar/**", "deep/foo/bar/baz/x"),
    (true, "**/bar/*/*", "deep/foo/bar/baz/x"),
    (true, "[[:alpha:]]", "a"),
    (false, "[[:alpha:]]", "1"),
    (false, "[[:digit:][:upper:][:space:]]", "a"),
    (true, "[[:digit:][:upper:][:space:]]", "A"),
    (true, "[[:digit:][:upper:][:space:]]", "1"),
    (false, "[[:digit:][:upper:][:spaci:]]", "1"),
    (true, "[[:digit:][:upper:][:space:]]", " "),
    (false, "[[:digit:][:upper:][:space:]]", "."),
    (true, "[[:digit:][:punct:][:space:]]", "."),
    (true, "[[:xdigit:]]", "5"),
    (true, "[[:xdigit:]]", "f"),
    (true, "[[:xdigit:]]", "D"),
    (true, "[a-c[:digit:]x-z]", "5"),
    (true, "[a-c[:digit:]x-z]", "b"),
    (true, "[a-c[:digit:]x-z]", "y"),
    (false, "[a-c[:digit:]x-z]", "q"),
    (true, r"[\\-^]", "]"),
    (false, r"[\\-^]", "["),
    (true, r"[\1-\3]", "2"),
    (true, r"[\1-\3]", "3"),
    (false, r"[\1-\3]", "4"),
    (true, r"[[-\]]", r"\"),
    (true, r"[[-\]]", "["),
    (true, r"[[-\]]", "]"),
    (false, r"[[-\]]", "-"),
    (true, "[Ww]ork", "work"),
    (true, "[Ww]ork", "Work"),
    (false, "[Ww]ork", "kork"),
    (true, "x[a-]y", "x-y"),
    (false, "[", "["),
    (false, "[a", "a"),
    (false, "[[:alpha:", "a"),
    (false, "a[", "a["),
    (true, "[[:]", ":"),
    (false, "[[:", "[:"),
    (true, "[[a]", "["),
    (true, "https://**/team/app.git", "https://team/app.git"),
    (true, "https://**/team/app.git", "https://a/b/team/app.git"),
    (false, "https://*/team/app.git", "https://a/b/team/app.git"),
    (
        true,
        "git@github.com:work/**",
        "git@github.com:work/team/app.git",
    ),
    (
        false,
        "git@github.com:work/*",
        "git@github.com:work/team/app.git",
    ),
    (false, "HTTPS://A/*", "https://a/b"),
];

#[test]
fn the_matcher_agrees_with_t3070_and_with_git() {
    for (expected, pattern, text) in WILDMATCH {
        assert_eq!(
            m(pattern, text),
            *expected,
            "t3070 says {expected} for pattern {pattern:?} against {text:?}"
        );
    }
    if !available() {
        eprintln!("git is not installed: matcher compared with the table only");
        return;
    }
    // `includeIf "hasconfig:remote.*.url:PATTERN"` is a `wildmatch` with
    // `WM_PATHNAME` of PATTERN against the remote's URL: one repository per
    // text, every pattern an include whose file sets a key named after it.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(dir.path(), &repo, &["init", "-q"]);
    let mut texts: Vec<&str> = WILDMATCH.iter().map(|(_, _, t)| *t).collect();
    texts.sort_unstable();
    texts.dedup();
    for text in texts {
        let mut config = String::new();
        for (k, (_, pattern, _)) in WILDMATCH.iter().enumerate() {
            let include = dir.path().join(format!("inc{k}"));
            std::fs::write(&include, format!("[m \"p{k}\"]\n\ty = 1\n")).unwrap();
            config.push_str(&format!(
                "[includeIf \"hasconfig:remote.*.url:{}\"]\n\tpath = {}\n",
                pattern.replace('\\', "\\\\"),
                include.display()
            ));
        }
        std::fs::write(dir.path().join(".gitconfig"), config).unwrap();
        let set = git(dir.path(), &repo, &["config", "remote.o.url", text]);
        assert!(set.status.success());
        let out = git(
            dir.path(),
            &repo,
            &["config", "--get-regexp", r"^m\..*\.y$"],
        );
        let matched: std::collections::HashSet<usize> = stdout(&out)
            .lines()
            .filter_map(|l| l.strip_prefix("m.p")?.split('.').next()?.parse().ok())
            .collect();
        for (k, (_, pattern, t)) in WILDMATCH.iter().enumerate() {
            if *t != text {
                continue;
            }
            assert_eq!(
                m(pattern, text),
                matched.contains(&k),
                "Git and devflow differ for pattern {pattern:?} against {text:?}"
            );
        }
    }
}

#[test]
fn case_folding_follows_git() {
    assert!(matches("*.EXAMPLE.com", "x.example.COM", true).unwrap());
    assert!(matches("[A-Z]ork", "work", true).unwrap());
    assert!(matches("[[:upper:]]ork", "work", true).unwrap());
    assert!(!matches("[[:upper:]]ork", "work", false).unwrap());
    // An escaped character is compared as written.
    assert!(!matches(r"\A", "a", true).unwrap());
    assert!(matches(r"\a", "A", true).unwrap());
}

#[test]
fn a_pathological_pattern_finishes_quickly() {
    let started = std::time::Instant::now();
    let pattern = format!("{}x", "*a".repeat(60));
    let text = "a".repeat(3000);
    assert!(!m(&pattern, &text));
    let deep = format!("{}z", "**/a/".repeat(40));
    assert!(!m(&deep, &"a/".repeat(1500)));
    let classes = format!("{}x", "[ab]*".repeat(60));
    assert!(!m(&classes, &"ab".repeat(1500)));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert!(matches(&"*".repeat(MAX_GLOB + 1), "x", false).is_err());
}
