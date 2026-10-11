//! Regression tests for the third round of code-review findings.
//!
//! Each test names the finding it pins down and uses the reviewer's fixture.
//! The threat model is the one in the module documentation: hostile
//! repository content must not make the scan leak credentials or do
//! unbounded work.
use super::hardening::{ignored, scan_within};
use super::round2::seal_sha1;
use super::*;
use std::time::Duration;

// -- 1. dotenv quoting variants ---------------------------------------------------------

/// Scan a repository whose `.env` is `text`, and check that `canary` is
/// nowhere in the facts while `PORT`, which comes after the string, still is.
fn assert_hidden(label: &str, text: &str, canary: &str) {
    let repo = fixture(&[(".env", text)]);
    let facts = scan_dir(&repo);

    assert!(
        !facts.env.has_key(canary),
        "{label}: {canary} is a key: {:?}",
        facts.env
    );
    assert!(!json(&facts).contains(canary), "{label}");
    assert!(!format!("{facts:?}").contains(canary), "{label}");
    assert!(facts.env.has_key("PORT"), "{label}: PORT was swallowed");
}

/// The reviewer's fixture: python-dotenv reads `\'` in a single-quoted value as
/// an escaped quote, so `SECRET` runs to the lone quote on the third line.
#[test]
fn r3_finding_1_an_escaped_quote_in_a_single_quoted_value_hides_the_next_lines() {
    assert_hidden(
        "escaped single quote",
        "SECRET='first\\'part\nDB_SECRET_CANARY=tail\n'\nPORT=1\n",
        "DB_SECRET_CANARY",
    );
    for quote in ['"', '\'', '`'] {
        assert_hidden(
            &format!("escaped {quote}"),
            &format!("SECRET={quote}first\\{quote}part\nDB_SECRET_CANARY=tail\n{quote}\nPORT=1\n"),
            "DB_SECRET_CANARY",
        );
        // Two backslashes before the quote: Python's `\\'` still reads the
        // quote as escaped.
        assert_hidden(
            &format!("doubled backslash, {quote}"),
            &format!("SECRET={quote}first\\\\{quote}\nDB_SECRET_CANARY=tail\n{quote}\nPORT=1\n"),
            "DB_SECRET_CANARY",
        );
    }
}

/// Python's `\s` matches vertical tab, form feed and the separator controls,
/// and Unicode spaces too: a quote after any of them opens the string.
#[test]
fn r3_finding_1_blank_space_before_the_quote_does_not_hide_the_string() {
    for blank in [
        '\u{b}', '\u{c}', '\u{1c}', '\u{1f}', '\u{85}', '\u{a0}', '\u{2003}', '\u{3000}',
    ] {
        for quote in ['"', '\''] {
            assert_hidden(
                &format!("{blank:?} before {quote}"),
                &format!("SECRET={blank}{quote}first\nDB_SECRET_CANARY=tail\n{quote}\nPORT=1\n"),
                "DB_SECRET_CANARY",
            );
        }
    }
    // A value that starts on the line after `NAME=` (Node's `dotenv`).
    assert_hidden(
        "next line",
        "SECRET=\n  '\nDB_SECRET_CANARY=tail\n'\nPORT=1\n",
        "DB_SECRET_CANARY",
    );
}

/// When the quote never closes unambiguously the rest of the file is the
/// value, and the scan says so.
#[test]
fn r3_finding_1_a_quote_that_never_closes_unambiguously_says_so() {
    let repo = fixture(&[(".env", "WIN='C:\\dir\\'\nDB_SECRET_CANARY=tail\nPORT=1\n")]);
    let facts = scan_dir(&repo);

    assert!(!json(&facts).contains("DB_SECRET_CANARY"));
    assert!(has_warning(&facts, "a quoted value is never closed"));
}

// -- 2. the checksum decides the hash size ----------------------------------------------

/// The reviewer's fixture: 44 bytes, version 2, no entries, a required
/// extension with 4 payload bytes, a valid SHA-1 trailer. Git rejects it. Read
/// as SHA-256 it looked like an empty index, so a tracked `.env` was reported
/// as ignored.
#[test]
fn r3_finding_2_a_crafted_index_with_a_required_extension_leaves_the_status_unknown() {
    let repo = fixture(&[
        ("package.json", "{}"),
        (".gitignore", ".env\n"),
        (".env", "A=1\n"),
    ]);
    git_init(repo.path());
    assert_eq!(ignored(&scan_dir(&repo), ".env"), Some(true), "baseline");

    for signature in [b"aaaa", b"link", b"sdir"] {
        let mut index = b"DIRC".to_vec();
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&0u32.to_be_bytes());
        index.extend_from_slice(signature);
        index.extend_from_slice(&4u32.to_be_bytes());
        index.extend_from_slice(&[0; 4]);
        let index = seal_sha1(index);
        assert_eq!(index.len(), 44);
        fs::write(repo.path().join(".git/index"), index).expect("write index");

        let facts = scan_within(repo.path(), Duration::from_secs(30));
        let name = String::from_utf8_lossy(signature);
        assert_eq!(ignored(&facts, ".env"), None, "{name}");
        assert!(has_warning(&facts, "split or sparse index"), "{name}");
    }
}

/// An index whose checksum is wrong is not one Git would read.
#[test]
fn r3_finding_2_an_index_with_a_wrong_checksum_leaves_the_status_unknown() {
    let repo = fixture(&[
        ("package.json", "{}"),
        (".gitignore", ".env\n"),
        (".env", "A=1\n"),
        ("README.md", "hi"),
    ]);
    let git = git2::Repository::init(repo.path()).expect("init");
    let mut index = git.index().expect("index");
    index.add_path(Path::new("README.md")).expect("add");
    index.write().expect("write index");
    assert_eq!(ignored(&scan_dir(&repo), ".env"), Some(true), "baseline");

    let path = repo.path().join(".git/index");
    let good = fs::read(&path).expect("read index");
    for at in [14, good.len() - 1] {
        let mut bad = good.clone();
        bad[at] ^= 1;
        fs::write(&path, bad).expect("write index");
        let facts = scan_dir(&repo);
        assert_eq!(ignored(&facts, ".env"), None, "byte {at}");
        assert!(has_warning(&facts, "damaged index"), "byte {at}");
    }
}

/// `index.skipHash` (turned on by `feature.manyFiles`) writes zeros for the
/// checksum, and Git reads such an index. Dropping it would make every
/// artifact of a big repository unknown.
#[test]
fn r3_finding_2_an_index_written_without_a_checksum_is_still_read() {
    let repo = fixture(&[
        ("package.json", "{}"),
        (".gitignore", ".env\n"),
        (".env", "A=1\n"),
        ("README.md", "hi"),
    ]);
    let git = git2::Repository::init(repo.path()).expect("init");
    let mut index = git.index().expect("index");
    index.add_path(Path::new("README.md")).expect("add");
    index.write().expect("write index");

    let path = repo.path().join(".git/index");
    let mut bytes = fs::read(&path).expect("read index");
    let length = bytes.len();
    bytes[length - 20..].fill(0);
    fs::write(&path, bytes).expect("write index");

    let facts = scan_dir(&repo);
    assert_eq!(ignored(&facts, ".env"), Some(true));
    assert!(!has_warning(&facts, "tracked files cannot be told"));
}

// -- 3. protected-file registration is charged for what it examines ----------------------

/// The reviewer's shape, at full size: 64 directories whose `.devflow` is a
/// link to one directory of 4096 dangling links. Each registration examined
/// 4096 entries and made 4096 lookups, and was charged for neither.
#[cfg(unix)]
#[test]
fn r3_finding_3_directories_of_dangling_links_exhaust_the_work_budget() {
    use std::os::unix::fs::symlink;
    let repo = fixture(&[("package.json", "{}")]);
    mkdir(repo.path(), "shared");
    for i in 0..4096 {
        symlink("nowhere", repo.path().join(format!("shared/l{i}"))).expect("symlink");
    }
    for i in 0..64 {
        mkdir(repo.path(), &format!("d{i}"));
        symlink("../shared", repo.path().join(format!("d{i}/.devflow"))).expect("symlink");
        write(repo.path(), &format!("d{i}/package.json"), "{}");
    }

    let facts = scan_within(repo.path(), Duration::from_secs(120));
    assert!(
        has_warning(&facts, "work budget for this scan is exhausted"),
        "{:?}",
        facts.warnings
    );
    assert!(
        has_warning(&facts, "before every file here was checked"),
        "{:?}",
        facts.warnings
    );
}

// -- 4. the process cap holds while processes are collected -----------------------------

#[test]
fn r3_finding_4_huge_procfiles_are_cut_while_they_are_read() {
    // About 900 KB each, under the size cap: 300,000 entries of three bytes.
    let procfile = "a:\n".repeat(300_000);
    let repo = fixture(&[
        ("Procfile", &procfile),
        ("Procfile.dev", &procfile),
        ("package.json", r#"{"scripts":{"dev":"vite"}}"#),
    ]);
    let facts = scan_within(repo.path(), Duration::from_secs(60));

    assert_eq!(facts.processes.len(), 80);
    assert!(
        facts
            .processes
            .iter()
            .all(|p| p.source == ProcessSource::Procfile)
    );
    assert!(has_warning(&facts, "limit of 80 processes"));
}

#[test]
fn r3_finding_4_the_cap_keeps_the_first_sources_and_stays_quiet_below_it() {
    let few = fixture(&[
        ("Procfile", "web: x\nworker: y\n"),
        ("package.json", r#"{"scripts":{"dev":"vite"}}"#),
    ]);
    let facts = scan_dir(&few);
    assert_eq!(facts.processes.len(), 3);
    assert!(!has_warning(&facts, "limit of"));

    // 79 Procfile entries, then the first of Procfile.dev; what comes later
    // (package scripts, mise tasks) is cut, in the order the sources are read.
    let procfile = "a:x\n".repeat(79);
    let dev = "b:x\nc:x\n";
    let many = fixture(&[
        ("Procfile", &procfile),
        ("Procfile.dev", dev),
        ("package.json", r#"{"scripts":{"dev":"vite"}}"#),
        ("mise.toml", "[tasks.dev]\nrun = 'x'\n"),
    ]);
    let facts = scan_dir(&many);
    assert_eq!(facts.processes.len(), 80);
    assert_eq!(facts.processes[78].name, "a");
    assert_eq!(facts.processes[79].name, "b");
    assert!(
        facts
            .processes
            .iter()
            .all(|p| p.source == ProcessSource::Procfile)
    );
    assert!(has_warning(&facts, "limit of 80 processes"));
}
