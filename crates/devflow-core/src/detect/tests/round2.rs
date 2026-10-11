//! Regression tests for the second round of code-review findings.
//!
//! Each test names the finding it pins down. The threat model is the one in
//! the module documentation: hostile repository content must not make the
//! scan leak credentials or do unbounded work, and a concurrent same-user
//! process is only met with cheap mitigations.
use super::hardening::{ignored, scan_within};
use super::*;
use std::time::Duration;

fn version_of(facts: &Facts, file: &str) -> Option<String> {
    facts
        .toolchain
        .version_files
        .iter()
        .find(|v| v.file == file)
        .unwrap_or_else(|| panic!("{file} is not a version file"))
        .version
        .clone()
}

// -- 1. a name checked is not a file opened ---------------------------------------------

/// The reviewer's reproduction: `.nvmrc` alternates between a regular file and
/// a symlink to `.devflow/env`. The scan used to check the name and then open
/// it, so now and then it opened the credentials and returned them as a
/// version. The content here is a single word so that it would pass the
/// version check: only the access layers stand in the way.
#[cfg(unix)]
#[test]
fn r2_finding_1_a_name_swapped_for_a_link_while_the_scan_runs_never_leaks() {
    use std::os::unix::fs::symlink;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const NAMES: [&str; 5] = [
        ".nvmrc",
        ".node-version",
        ".python-version",
        ".ruby-version",
        "rust-toolchain",
    ];
    let repo = fixture(&[
        (".devflow/env", "RACE_CANARY\n"),
        ("package.json", "{}"),
        ("deep/inner/.env", "RACE_CANARY\n"),
    ]);
    for name in NAMES {
        write(repo.path(), name, "22\n");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let (root, stop) = (repo.path().to_path_buf(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let scratch = root.join(".swap");
            let mut round = 0usize;
            while !stop.load(Ordering::Relaxed) {
                round += 1;
                for name in NAMES {
                    let _ = std::fs::remove_file(&scratch);
                    // Both a link to the managed env file and a link to a
                    // dotenv file in a directory the scan never lists.
                    match round % 3 {
                        0 => {
                            let _ = symlink(".devflow/env", &scratch);
                        }
                        1 => {
                            let _ = symlink("deep/inner/.env", &scratch);
                        }
                        _ => {
                            let _ = std::fs::write(&scratch, "22\n");
                        }
                    }
                    let _ = std::fs::rename(&scratch, root.join(name));
                }
            }
        })
    };

    let root = repo.path().to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..600 {
            let facts = scan(&root).expect("scan");
            let text = serde_json::to_string(&facts).expect("json");
            if text.contains("RACE_CANARY") {
                let _ = tx.send(Err(text));
                return;
            }
        }
        let _ = tx.send(Ok(()));
    });
    let outcome = rx.recv_timeout(Duration::from_secs(120));
    stop.store(true, Ordering::Relaxed);
    swapper.join().expect("swapper");
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(text)) => panic!("protected content reached the facts: {text}"),
        Err(_) => panic!("the scan did not finish"),
    }
}

// -- 2. hard links and what counts as a version ---------------------------------------

#[cfg(unix)]
#[test]
fn r2_finding_2_a_file_with_another_name_is_never_read_as_text() {
    let repo = fixture(&[
        // Directories the scan never lists, so no listing has met these files.
        ("node_modules/.env", "HARDLINK_CANARY\n"),
        (
            "deep/inner/.devflow/env",
            "DATABASE_URL=postgresql://app:LINKED_CANARY@db/app\n", // kingfisher:ignore
        ),
        // The managed env file at the top, which the scan does list.
        (
            ".devflow/env",
            "DATABASE_URL=postgresql://app:TOOLS_CANARY@db/app\n", // kingfisher:ignore
        ),
        ("package.json", "{}"),
    ]);
    let root = repo.path();
    fs::hard_link(root.join("node_modules/.env"), root.join(".nvmrc")).expect("link");
    fs::hard_link(
        root.join("deep/inner/.devflow/env"),
        root.join(".python-version"),
    )
    .expect("link");
    fs::hard_link(root.join(".devflow/env"), root.join(".tool-versions")).expect("link");
    let facts = scan_dir(&repo);

    let text = json(&facts);
    for canary in ["HARDLINK_CANARY", "LINKED_CANARY", "TOOLS_CANARY"] {
        assert!(!text.contains(canary), "{canary} leaked: {facts:#?}");
    }
    assert_eq!(version_of(&facts, ".nvmrc"), None);
    assert_eq!(version_of(&facts, ".python-version"), None);
    let mise = facts.toolchain.mise.as_ref().expect("mise config file");
    assert_eq!(mise.config_files, vec![".tool-versions"]);
    assert!(mise.tools.is_empty(), "{:?}", mise.tools);
    assert!(
        facts
            .warnings
            .iter()
            .any(|w| w.path == ".nvmrc" && w.message.contains("hard links")),
        "{:?}",
        facts.warnings
    );
}

#[cfg(unix)]
#[test]
fn r2_finding_2_a_file_read_by_a_general_reader_must_have_a_single_name() {
    use std::os::unix::fs::MetadataExt;
    let repo = fixture(&[("package.json", r#"{"dependencies":{"next":"15"}}"#)]);
    fs::hard_link(
        repo.path().join("package.json"),
        repo.path().join("elsewhere.json"),
    )
    .expect("link");
    assert_eq!(
        fs::metadata(repo.path().join("package.json"))
            .expect("stat")
            .nlink(),
        2
    );
    let facts = scan_dir(&repo);
    assert!(!facts.has_framework(Framework::NextJs));
    assert!(has_warning(&facts, "hard links"));
}

#[test]
fn r2_finding_2_only_version_like_tokens_become_versions() {
    for (value, expected) in [
        ("22", Some("22")),
        ("v22.1.0", Some("v22.1.0")),
        ("lts/iron", Some("lts/iron")),
        ("lts/*", Some("lts/*")),
        ("20.x", Some("20.x")),
        ("3.12.4", Some("3.12.4")),
        ("pypy3.10-7.3.15", Some("pypy3.10-7.3.15")),
        ("nightly-2024-05-01", Some("nightly-2024-05-01")),
        ("  22  ", Some("22")),
        ("postgresql://app:VERSION_CANARY@db/app", None), // kingfisher:ignore
        ("TOKEN=VERSION_CANARY", None),
        ("user@VERSION_CANARY", None),
        ("a:VERSION_CANARY", None),
        ("two VERSION_CANARY", None),
        ("22 VERSION_CANARY", None),
        ("-VERSION_CANARY", None),
        (".VERSION_CANARY", None),
        ("/VERSION_CANARY", None),
        (&"v".repeat(65), None),
    ] {
        let repo = fixture(&[(".nvmrc", &format!("# pinned\n{value}\n"))]);
        let facts = scan_dir(&repo);

        assert_eq!(
            version_of(&facts, ".nvmrc").as_deref(),
            expected,
            "{value:?}"
        );
        assert!(!json(&facts).contains("VERSION_CANARY"), "{value:?}");
    }
    // The same rules apply to the channel of rust-toolchain.toml.
    let repo = fixture(&[(
        "rust-toolchain.toml",
        "[toolchain]\nchannel = \"stable VERSION_CANARY\"\n",
    )]);
    let facts = scan_dir(&repo);
    assert_eq!(version_of(&facts, "rust-toolchain.toml"), None);
    assert!(!json(&facts).contains("VERSION_CANARY"));
}

#[test]
fn r2_finding_2_tool_versions_entries_must_be_a_tool_and_versions() {
    let repo = fixture(&[(
        ".tool-versions",
        concat!(
            "nodejs 22.1.0 20 # a comment\n",
            "DATABASE_URL=postgresql://app:TOOL_CANARY@db/app\n", // kingfisher:ignore
            "TOOL_CANARY_ALONE\n",
            "TOKEN: TOOL_CANARY_COLON\n",
            "user@TOOL_CANARY_AT 1\n",
            "npm:prettier 3.0.0\n",
            "python system ref:main\n",
            "ruby 3.3.0 TOOL_CANARY=2\n",
        ),
    )]);
    let facts = scan_dir(&repo);

    let mise = facts.toolchain.mise.as_ref().expect("mise");
    assert_eq!(mise.tools, vec!["nodejs", "npm:prettier", "python"]);
    assert!(!json(&facts).contains("TOOL_CANARY"), "{facts:#?}");
    let warnings: Vec<_> = facts
        .warnings
        .iter()
        .filter(|w| w.path == ".tool-versions")
        .collect();
    assert!(!warnings.is_empty());
    assert!(
        warnings.iter().all(|w| !w.message.contains("CANARY")),
        "{warnings:?}"
    );
}

// -- 3. a git index cannot be made to expand without bound ------------------------------

/// A version 4 entry (SHA-1 object names) that drops `strip` bytes of the
/// previous path and appends `suffix`.
fn v4_entry(strip: u8, suffix: &[u8]) -> Vec<u8> {
    let mut entry = vec![0u8; 40 + 20 + 2];
    entry[24..28].copy_from_slice(&0o100_644u32.to_be_bytes());
    entry.push(strip);
    entry.extend_from_slice(suffix);
    entry.push(0);
    entry
}

/// `body` followed by its SHA-1, the checksum that ends every index Git writes.
pub(super) fn seal_sha1(mut body: Vec<u8>) -> Vec<u8> {
    let sum = sha1_smol::Sha1::from(&body).digest().bytes();
    body.extend_from_slice(&sum);
    body
}

fn v4_index(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut index = b"DIRC".to_vec();
    index.extend_from_slice(&4u32.to_be_bytes());
    index.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    entries.iter().for_each(|e| index.extend_from_slice(e));
    seal_sha1(index)
}

#[test]
fn r2_finding_3_an_index_that_expands_beyond_the_limits_leaves_the_status_unknown() {
    let repo = fixture(&[
        ("package.json", "{}"),
        (".gitignore", ".env\n"),
        (".env", "A=1\n"),
    ]);
    git_init(repo.path());
    assert_eq!(ignored(&scan_dir(&repo), ".env"), Some(true), "baseline");

    // About 36 MB of paths, none longer than the per-path cap, from 600 KB of index.
    let mut many = vec![v4_entry(0, &[b'a'; 4000])];
    for i in 0..9000usize {
        many.push(v4_entry(2, &[1 + (i % 250) as u8, 1 + (i / 250) as u8]));
    }
    // The reviewer's shape: a path far past the cap, then entries that keep it.
    let mut long = vec![v4_entry(0, &[b'a'; 20_000])];
    for i in 0..1500usize {
        long.push(v4_entry(0, &[1 + (i % 250) as u8]));
    }
    for (name, entries) in [("many paths", many), ("one long path", long)] {
        let index = v4_index(&entries);
        assert!(index.len() < 700_000, "{name}: {} bytes", index.len());
        fs::write(repo.path().join(".git/index"), index).expect("write index");

        let facts = scan_within(repo.path(), Duration::from_secs(30));
        assert_eq!(ignored(&facts, ".env"), None, "{name}");
        assert!(has_warning(&facts, "limits of the scan"), "{name}");
    }
}

// -- 4. no command text from the repository --------------------------------------------

#[test]
fn r2_finding_4_procfile_commands_are_not_in_the_facts_at_all() {
    let repo = fixture(&[
        (
            "Procfile",
            concat!(
                "web: service --password FLAG_CANARY --port 3000\n", // kingfisher:ignore
                "cache: redis-server --requirepass REDIS_CANARY\n",  // kingfisher:ignore
                "worker: env TOKEN=PLAIN_CANARY bundle exec sidekiq\n", // kingfisher:ignore
            ),
        ),
        (
            "package.json",
            r#"{"workspaces":["apps/*"],"scripts":{"dev":"next dev --password SCRIPT_CANARY"}}"#,
        ),
        ("apps/web/package.json", r#"{"name":"web"}"#),
        (
            "apps/web/Procfile.dev",
            "assets: webpack --token ASSET_CANARY\n", // kingfisher:ignore
        ),
        (
            "mise.toml",
            "[tasks.dev]\nrun = 'serve --secret MISE_CANARY'\n", // kingfisher:ignore
        ),
    ]);
    let facts = scan_dir(&repo);

    for text in [json(&facts), format!("{facts:?}")] {
        for canary in [
            "FLAG_CANARY",
            "REDIS_CANARY",
            "PLAIN_CANARY",
            "SCRIPT_CANARY",
            "ASSET_CANARY",
            "MISE_CANARY",
            "--password",
            "requirepass",
        ] {
            assert!(!text.contains(canary), "{canary} in {text}");
        }
    }
    for (name, display) in [
        ("web", "Procfile: web"),
        ("cache", "Procfile: cache"),
        ("worker", "Procfile: worker"),
    ] {
        let found = process(&facts, name, ProcessSource::Procfile).expect(name);
        assert_eq!(found.display.as_deref(), Some(display));
        assert_eq!(
            found.launch,
            Some(ProcessLaunch::Procfile {
                file: "Procfile".into(),
                process: name.into()
            })
        );
    }
    let assets = process(&facts, "assets", ProcessSource::Procfile).expect("assets");
    assert_eq!(
        assets.display.as_deref(),
        Some("apps/web/Procfile.dev: assets")
    );
    // Every other source is an argument vector of names fixed by the scan.
    let dev = process(&facts, "dev", ProcessSource::PackageJson).expect("dev script");
    assert_eq!(dev.display.as_deref(), Some("npm run dev"));
    let mise = process(&facts, "dev", ProcessSource::MiseTask).expect("mise task");
    assert_eq!(mise.display.as_deref(), Some("mise run dev"));
}

// -- 5. a quote opens a string whatever the name before it ------------------------------

#[test]
fn r2_finding_5_a_rejected_name_still_opens_its_quoted_value() {
    let long_name = "A".repeat(257);
    let huge_name = "B".repeat(5000);
    for (label, text) in [
        (
            "byte-order mark",
            "\u{feff}SECRET=\"line1\nDB_BOM_CANARY=x\n\"\nPOSTGRES_USER=u\n".to_string(),
        ),
        (
            "byte-order mark on an invalid name",
            "\u{feff}BAD NAME=\"line1\nDB_BOM_CANARY=x\n\"\nPOSTGRES_USER=u\n".to_string(),
        ),
        (
            "a mark in the middle of a name",
            "OK=1\nBAD\u{feff}NAME='line1\nDB_BOM_CANARY=x\n'\nPOSTGRES_USER=u\n".to_string(),
        ),
        (
            "257-character name",
            format!("{long_name}=\"v\nDB_BOM_CANARY=x\n\"\nPOSTGRES_USER=u\n"),
        ),
        (
            "5000-character name",
            format!("{huge_name}=`v\nDB_BOM_CANARY=x\n`\nPOSTGRES_USER=u\n"),
        ),
        (
            "digit-first name",
            "1BAD=\"v\nDB_BOM_CANARY=x\n\"\nPOSTGRES_USER=u\n".to_string(),
        ),
    ] {
        let repo = fixture(&[(".env", &text)]);
        let facts = scan_dir(&repo);

        assert!(
            !facts.env.has_key("DB_BOM_CANARY"),
            "{label}: {:?}",
            facts.env
        );
        assert!(!json(&facts).contains("DB_BOM_CANARY"), "{label}");
        assert!(facts.env.has_key("POSTGRES_USER"), "{label}");
    }
}

#[test]
fn r2_finding_5_a_leading_byte_order_mark_is_not_part_of_the_first_name() {
    let repo = fixture(&[(".env", "\u{feff}DATABASE_URL=x\nPORT=1\n")]);
    let facts = scan_dir(&repo);

    assert!(facts.env.has_key("DATABASE_URL"));
    assert_eq!(facts.env.files[0].key_count, 2);
    assert!(facts.env.has_database_keys);
}

// -- 6. split indexes ----------------------------------------------------------------

#[test]
fn r2_finding_6_a_split_index_leaves_the_status_unknown() {
    let repo = fixture(&[
        ("package.json", "{}"),
        (".gitignore", ".env*\nnode_modules\n"),
        (".env", "A=1\n"),
        ("README.md", "hi"),
        ("node_modules/.keep", ""),
    ]);
    let git = git2::Repository::init(repo.path()).expect("init");
    let mut index = git.index().expect("index");
    index.add_path(Path::new("README.md")).expect("add");
    index.write().expect("write index");
    let facts = scan_dir(&repo);
    assert_eq!(ignored(&facts, ".env"), Some(true), "baseline");

    // `git update-index --split-index` moves the entries to a shared index
    // file and leaves a `link` extension behind. Say a force-added `.env`
    // lives there: the main index no longer lists it.
    let path = repo.path().join(".git/index");
    let mut bytes = fs::read(&path).expect("read index");
    bytes.truncate(bytes.len() - 20);
    bytes.extend_from_slice(b"link");
    bytes.extend_from_slice(&40u32.to_be_bytes());
    bytes.extend_from_slice(&[0x11; 20]); // checksum of the shared index
    bytes.extend_from_slice(&[0; 20]); // delete and replace bitmaps
    fs::write(&path, seal_sha1(bytes)).expect("write index");

    let facts = scan_dir(&repo);
    assert_eq!(ignored(&facts, ".env"), None);
    assert!(has_warning(&facts, "split or sparse index"));
    // Directories ignored by a rule do not depend on the index.
    assert_eq!(ignored(&facts, "node_modules"), Some(true));
}

// -- 7. an external git directory is only trusted when it belongs to this checkout -------

#[test]
fn r2_finding_7_a_git_pointer_to_an_unrelated_directory_imports_nothing() {
    let elsewhere = tempfile::tempdir().expect("tempdir");
    fs::write(elsewhere.path().join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    fs::create_dir_all(elsewhere.path().join("info")).expect("info");
    fs::write(
        elsewhere.path().join("info/exclude"),
        "node_modules\n.env\n",
    )
    .expect("exclude");

    let repo = fixture(&[
        ("package.json", "{}"),
        ("node_modules/.keep", ""),
        (".env", "A=1\n"),
    ]);
    fs::write(
        repo.path().join(".git"),
        format!("gitdir: {}\n", elsewhere.path().display()),
    )
    .expect("write .git");
    let facts = scan_dir(&repo);

    assert!(facts.git.repository, "it is still a checkout");
    assert_eq!(ignored(&facts, "node_modules"), None);
    assert_eq!(ignored(&facts, ".env"), None);
    assert!(
        has_warning(&facts, "cannot be tied to this checkout"),
        "{:?}",
        facts.warnings
    );
}

#[test]
fn r2_finding_7_a_separate_git_directory_is_unknown_not_guessed() {
    let work = tempfile::tempdir().expect("work tree");
    let gitdir = tempfile::tempdir().expect("git directory");
    let mut options = git2::RepositoryInitOptions::new();
    options.no_dotgit_dir(true).workdir_path(work.path());
    git2::Repository::init_opts(gitdir.path(), &options).expect("init");
    assert!(work.path().join(".git").is_file(), "a gitlink file");
    for (path, content) in [
        ("package.json", "{}"),
        (".gitignore", "node_modules\n"),
        ("node_modules/.keep", ""),
    ] {
        write(work.path(), path, content);
    }
    let facts = scan(work.path()).expect("scan");

    assert!(facts.git.repository);
    assert_eq!(ignored(&facts, "node_modules"), None);
    assert!(has_warning(&facts, "cannot be tied to this checkout"));
}
