//! Regression tests for the code-review findings on the detection module.
//!
//! Each test names the finding it pins down. They exercise the scan end to
//! end on hostile repositories: races, aliases, oversized or malicious inputs.
use super::*;
use std::time::{Duration, Instant};

/// Run `scan` on a helper thread so a scan that blocks fails the test instead
/// of hanging it.
pub(super) fn scan_within(root: &Path, limit: Duration) -> Facts {
    let root = root.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(scan(&root));
    });
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| panic!("the scan did not finish within {limit:?}"))
        .expect("scan")
}

#[cfg(unix)]
fn mkfifo(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).expect("path");
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
}

// -- 1. race-resistant access -----------------------------------------------------------

/// A file, or a directory on the way to one, replaced by a link out of the
/// repository or by a FIFO between the moment the scan looks at it and the
/// moment it opens it: the old stat-then-open code followed the link or hung.
#[cfg(unix)]
#[test]
fn finding_1_files_and_directories_swapped_mid_scan_are_never_followed_or_waited_on() {
    use std::os::unix::fs::symlink;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let outside = fixture(&[
        (
            "package.json",
            r#"{"name":"CANARY_OUTSIDE","dependencies":{"next":"15"}}"#,
        ),
        (".env", "CANARY_OUTSIDE_DATABASE_URL=v\n"),
        ("mise.toml", "[tasks.\"dev:CANARY_OUTSIDE\"]\nrun = 'x'\n"),
        (
            "compose.yaml",
            "services:\n  CANARY_OUTSIDE:\n    image: postgres:16\n",
        ),
        (
            "apps/web/package.json",
            r#"{"name":"CANARY_OUTSIDE_WEB","dependencies":{"next":"15"}}"#,
        ),
    ]);
    let inside_files = [
        (
            "package.json",
            r#"{"name":"inside","workspaces":["apps/*"]}"#,
        ),
        (".env", "INSIDE_DATABASE_URL=v\n"),
        ("mise.toml", "[tools]\nnode = '22'\n"),
        ("compose.yaml", "services:\n  db:\n    image: redis\n"),
        ("apps/web/package.json", r#"{"name":"web"}"#),
    ];
    let repo = fixture(&inside_files);
    let stash = fixture(&inside_files);

    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let (root, outside, stash, stop) = (
            repo.path().to_path_buf(),
            outside.path().to_path_buf(),
            stash.path().to_path_buf(),
            Arc::clone(&stop),
        );
        std::thread::spawn(move || {
            let scratch = root.join(".swap");
            let mut round = 0usize;
            while !stop.load(Ordering::Relaxed) {
                round += 1;
                for name in ["package.json", ".env", "mise.toml", "compose.yaml"] {
                    let _ = std::fs::remove_file(&scratch);
                    match round % 3 {
                        0 => {
                            let _ = symlink(outside.join(name), &scratch);
                        }
                        1 => mkfifo(&scratch),
                        _ => {
                            let _ = std::fs::copy(stash.join(name), &scratch);
                        }
                    }
                    let _ = std::fs::rename(&scratch, root.join(name));
                }
                // An intermediate directory becomes a link out, and back.
                if round.is_multiple_of(2) {
                    let _ = std::fs::rename(root.join("apps"), root.join("apps.real"));
                    let _ = symlink(outside.join("apps"), root.join("apps"));
                } else if root.join("apps.real").exists() {
                    let _ = std::fs::remove_file(root.join("apps"));
                    let _ = std::fs::rename(root.join("apps.real"), root.join("apps"));
                }
            }
        })
    };

    let root = repo.path().to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..400 {
            let facts = scan(&root).expect("scan");
            let text = serde_json::to_string(&facts).expect("json");
            if text.contains("CANARY_OUTSIDE") {
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
        Ok(Err(text)) => panic!("content from outside the repository reached the facts: {text}"),
        Err(_) => panic!("the scan blocked on a swapped-in FIFO"),
    }
}

// -- 2. aliases of dotenv files --------------------------------------------------------

#[cfg(unix)]
#[test]
fn finding_2_aliases_of_protected_files_are_never_read_as_text() {
    use std::os::unix::fs::symlink;
    let repo = fixture(&[
        (
            ".env",
            "DATABASE_URL=postgresql://app:ALIAS_MUST_NOT_LEAK@db/app\n",
        ),
        ("sub/.env.production", "SECRET_TOKEN=ALIAS_MUST_NOT_LEAK\n"),
        (".devflow/env", "DATABASE_URL=ALIAS_MUST_NOT_LEAK\n"),
        ("real/notes.txt", "22\n"),
    ]);
    let root = repo.path();
    symlink(".env", root.join(".nvmrc")).expect("symlink");
    // A target in a directory the scan never lists.
    symlink("sub/.env.production", root.join(".node-version")).expect("symlink");
    symlink(".devflow/env", root.join(".python-version")).expect("symlink");
    symlink("real/../.env", root.join("rust-toolchain")).expect("symlink");
    // A hard link carries no name to check: the file identity is what matters.
    fs::hard_link(root.join(".env"), root.join(".ruby-version")).expect("hard link");
    // Other readers get the same treatment.
    symlink(".env", root.join("compose.yaml")).expect("symlink");
    symlink(".env", root.join("mise.toml")).expect("symlink");
    symlink(".devflow/env", root.join("Procfile")).expect("symlink");
    symlink(".env", root.join("package.json")).expect("symlink");
    let facts = scan_dir(&repo);

    assert!(!json(&facts).contains("ALIAS_MUST_NOT_LEAK"), "{facts:#?}");
    for version in &facts.toolchain.version_files {
        assert_eq!(version.version, None, "{}", version.file);
    }
    assert_eq!(facts.toolchain.version_files.len(), 5);
    assert!(facts.compose.is_none());
    assert!(facts.js.is_empty());
    assert!(
        facts
            .toolchain
            .mise
            .as_ref()
            .is_none_or(|m| m.tools.is_empty() && m.tasks.is_empty() && m.env_files.is_empty())
    );
    assert!(facts.processes.is_empty());
    assert!(
        has_warning(&facts, "only its variable names are read"),
        "{:?}",
        facts.warnings
    );
    // The keys-only reader still sees the real files.
    assert!(facts.env.has_key("DATABASE_URL"));
}

#[test]
fn finding_2_a_version_file_must_look_like_a_version() {
    let repo = fixture(&[
        (".nvmrc", "postgresql://app:VERSION_MUST_NOT_LEAK@db/app\n"),
        (".python-version", "3.12.4\n"),
    ]);
    let facts = scan_dir(&repo);

    assert!(!json(&facts).contains("VERSION_MUST_NOT_LEAK"));
    let version = |name: &str| {
        facts
            .toolchain
            .version_files
            .iter()
            .find(|v| v.file == name)
            .and_then(|v| v.version.clone())
    };
    assert_eq!(version(".nvmrc"), None);
    assert_eq!(version(".python-version").as_deref(), Some("3.12.4"));
}

// -- 3. git without libgit2 -------------------------------------------------------------

/// Repository with an ignored `node_modules` and `.env` (by `.gitignore`) and
/// `vendor` (by `info/exclude` only), ready for a git-file tweak.
fn ignoring_repo() -> TempDir {
    let repo = fixture(&[
        ("package.json", "{}"),
        ("composer.json", "{}"),
        (".gitignore", "node_modules\n.env*\n"),
        ("node_modules/.keep", ""),
        ("vendor/.keep", ""),
        (".env", "A=1\n"),
    ]);
    git_init(repo.path());
    fs::write(repo.path().join(".git/info/exclude"), "vendor\n").expect("exclude");
    repo
}

pub(super) fn ignored(facts: &Facts, path: &str) -> Option<bool> {
    artifact(facts, path)
        .unwrap_or_else(|| panic!("{path} is not an artifact"))
        .git_ignored
}

#[cfg(unix)]
#[test]
fn finding_3_a_fifo_in_any_git_input_does_not_block_the_scan() {
    // A baseline: everything regular.
    let facts = scan_dir(&ignoring_repo());
    assert_eq!(ignored(&facts, "node_modules"), Some(true));
    assert_eq!(ignored(&facts, ".env"), Some(true));
    assert_eq!(ignored(&facts, "vendor"), Some(true));

    for (fifo, node_modules, env, vendor) in [
        (".gitignore", None, None, None),
        // Lower precedence than .gitignore: only `vendor` depends on it.
        (".git/info/exclude", Some(true), Some(true), None),
        // The config is not read at all.
        (".git/config", Some(true), Some(true), Some(true)),
        // Only needed to tell a tracked file from an ignored one.
        (".git/index", Some(true), None, Some(true)),
    ] {
        let repo = ignoring_repo();
        let path = repo.path().join(fifo);
        let _ = fs::remove_file(&path);
        mkfifo(&path);
        let facts = scan_within(repo.path(), Duration::from_secs(20));

        assert!(facts.git.repository, "{fifo}");
        assert_eq!(ignored(&facts, "node_modules"), node_modules, "{fifo}");
        assert_eq!(ignored(&facts, ".env"), env, "{fifo}");
        assert_eq!(ignored(&facts, "vendor"), vendor, "{fifo}");
    }
}

#[cfg(unix)]
#[test]
fn finding_3_a_fifo_as_head_is_not_a_git_directory() {
    let repo = ignoring_repo();
    let head = repo.path().join(".git/HEAD");
    fs::remove_file(&head).expect("remove HEAD");
    mkfifo(&head);
    let facts = scan_within(repo.path(), Duration::from_secs(20));

    assert!(!facts.git.repository);
    assert_eq!(ignored(&facts, "node_modules"), None);
}

#[test]
fn finding_3_git_config_includes_and_global_excludes_are_not_followed() {
    let outside = fixture(&[("excludes", "node_modules\n.env*\n")]);
    let repo = fixture(&[
        ("package.json", "{}"),
        ("node_modules/.keep", ""),
        (".env", "A=1\n"),
    ]);
    git_init(repo.path());
    let config = repo.path().join(".git/config");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "[core]\n\texcludesFile = {0}\n[include]\n\tpath = {0}\n",
        outside.path().join("excludes").display()
    ));
    fs::write(&config, text).expect("write config");
    let facts = scan_dir(&repo);

    // libgit2 honored `core.excludesFile` and would answer `Some(true)`.
    assert_eq!(ignored(&facts, "node_modules"), Some(false));
    assert_eq!(ignored(&facts, ".env"), Some(false));
}

#[cfg(unix)]
#[test]
fn finding_3_gitignore_files_cannot_be_links() {
    use std::os::unix::fs::symlink;
    // Out of the repository: the target is never read.
    let outside = fixture(&[("ignore", "node_modules\n")]);
    let repo = fixture(&[("package.json", "{}"), ("node_modules/.keep", "")]);
    git_init(repo.path());
    symlink(
        outside.path().join("ignore"),
        repo.path().join(".gitignore"),
    )
    .expect("symlink");
    let facts = scan_dir(&repo);
    assert_eq!(ignored(&facts, "node_modules"), Some(false));

    // Inside the repository: Git (2.32 and later) skips symlinked ignore files.
    let repo = fixture(&[
        ("package.json", "{}"),
        ("node_modules/.keep", ""),
        ("shared.ignore", "node_modules\n"),
    ]);
    git_init(repo.path());
    symlink("shared.ignore", repo.path().join(".gitignore")).expect("symlink");
    let facts = scan_dir(&repo);
    assert_eq!(ignored(&facts, "node_modules"), Some(false));
}

#[test]
fn finding_3_unreadable_inputs_make_the_status_unknown_not_false() {
    // Over the size cap.
    let repo = fixture(&[
        ("package.json", "{}"),
        ("node_modules/.keep", ""),
        (
            ".gitignore",
            &format!("{}\nnode_modules\n", "# x".repeat(500_000)),
        ),
    ]);
    git_init(repo.path());
    let facts = scan_dir(&repo);
    assert_eq!(ignored(&facts, "node_modules"), None);
    assert!(has_warning(&facts, "larger than 1024 KiB"));

    // Too many patterns.
    let many: String = (0..12_000).map(|i| format!("generated-{i}/\n")).collect();
    let repo = fixture(&[
        ("package.json", "{}"),
        ("node_modules/.keep", ""),
        (".gitignore", &many),
    ]);
    git_init(repo.path());
    let facts = scan_dir(&repo);
    assert_eq!(ignored(&facts, "node_modules"), None);
    assert!(has_warning(&facts, "Git ignore status is unavailable"));

    // A damaged index only matters when a rule matched a file.
    let repo = ignoring_repo();
    fs::write(repo.path().join(".git/index"), b"DIRC garbage").expect("write index");
    let facts = scan_dir(&repo);
    assert_eq!(ignored(&facts, "node_modules"), Some(true));
    assert_eq!(ignored(&facts, ".env"), None);
    assert!(has_warning(&facts, "unsupported or damaged index"));
}

#[test]
fn finding_3_ignore_rules_follow_git_precedence() {
    let repo = fixture(&[
        ("package.json", r#"{"workspaces":["apps/*"]}"#),
        // A later pattern re-includes one member's directory.
        (".gitignore", "node_modules/\n!/apps/api/node_modules/\n"),
        ("node_modules/.keep", ""),
        ("apps/web/package.json", "{}"),
        ("apps/web/node_modules/.keep", ""),
        ("apps/api/package.json", "{}"),
        ("apps/api/node_modules/.keep", ""),
        // $GIT_DIR/info/exclude.
        ("composer.json", "{}"),
        ("vendor/autoload.php", ""),
        // A .gitignore inside the directory ignores its contents.
        ("tmp/.keep", ""),
        ("tmp/.gitignore", "*\n!.gitignore\n"),
        // Not ignored by anything.
        ("storage/app/.keep", ""),
    ]);
    git_init(repo.path());
    fs::write(
        repo.path().join(".git/info/exclude"),
        "# local only\n/vendor/\n",
    )
    .expect("exclude");
    let facts = scan_dir(&repo);

    assert_eq!(ignored(&facts, "node_modules"), Some(true));
    assert_eq!(ignored(&facts, "apps/web/node_modules"), Some(true));
    assert_eq!(ignored(&facts, "apps/api/node_modules"), Some(false));
    assert_eq!(ignored(&facts, "vendor"), Some(true));
    assert_eq!(ignored(&facts, "tmp"), Some(true));
    assert_eq!(ignored(&facts, "storage"), Some(false));

    // Nothing inside an ignored directory can be re-included.
    let repo = fixture(&[
        ("package.json", r#"{"workspaces":["apps/*"]}"#),
        (".gitignore", "/apps/web/\n"),
        ("apps/web/package.json", "{}"),
        ("apps/web/.gitignore", "!node_modules\n"),
        ("apps/web/node_modules/.keep", ""),
    ]);
    git_init(repo.path());
    let facts = scan_dir(&repo);
    assert_eq!(ignored(&facts, "apps/web/node_modules"), Some(true));
}

/// A linked worktree has a `.git` file; its rules and index live in the main
/// repository's git directory.
#[test]
fn finding_3_linked_worktrees_are_inspected_through_their_git_directory() {
    let main = tempfile::tempdir().expect("tempdir");
    let parent = tempfile::tempdir().expect("tempdir");
    let wt = parent.path().join("wt");
    let repo = git2::Repository::init(main.path()).expect("init");
    fs::write(main.path().join("README.md"), "x").expect("write");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("README.md")).expect("add");
    index.write().expect("write index");
    let tree = repo
        .find_tree(index.write_tree().expect("tree"))
        .expect("find tree");
    let signature = git2::Signature::now("T", "t@example.com").expect("signature");
    repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
        .expect("commit");
    repo.worktree("wt", &wt, None).expect("worktree");
    // The main repository's shared exclude file, and the worktree's own files.
    fs::write(main.path().join(".git/info/exclude"), "node_modules\n").expect("exclude");
    for (path, content) in [
        ("package.json", "{}"),
        (".gitignore", ".env*\n"),
        ("node_modules/.keep", ""),
        (".env", "A=1\n"),
        (".env.local", "A=1\n"),
    ] {
        write(&wt, path, content);
    }
    // Track `.env` in the worktree's index (which is not the main one).
    let linked = git2::Repository::open(&wt).expect("open worktree");
    let mut linked_index = linked.index().expect("worktree index");
    linked_index.add_path(Path::new(".env")).expect("add .env");
    linked_index.write().expect("write worktree index");
    assert!(
        wt.join(".git").is_file(),
        "a linked worktree has a .git file"
    );
    let facts = scan(&wt).expect("scan");

    assert!(facts.git.repository);
    assert_eq!(ignored(&facts, "node_modules"), Some(true));
    assert_eq!(
        ignored(&facts, ".env"),
        Some(false),
        "tracked in the worktree index"
    );
    assert_eq!(ignored(&facts, ".env.local"), Some(true));
}

#[test]
fn finding_3_a_git_file_is_only_trusted_when_it_checks_out() {
    let main = tempfile::tempdir().expect("tempdir");
    let repo = git2::Repository::init(main.path()).expect("init");
    let signature = git2::Signature::now("T", "t@example.com").expect("signature");
    let tree = repo
        .find_tree(repo.index().expect("index").write_tree().expect("tree"))
        .expect("find tree");
    repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
        .expect("commit");
    let parent = tempfile::tempdir().expect("tempdir");
    repo.worktree("real", &parent.path().join("real"), None)
        .expect("worktree");
    let admin = main.path().join(".git/worktrees/real");

    // A directory that claims another worktree's git directory: the link back
    // does not name it, so ignore status stays unknown.
    let imposter = fixture(&[("package.json", "{}"), ("node_modules/.keep", "")]);
    fs::write(
        imposter.path().join(".git"),
        format!("gitdir: {}\n", admin.display()),
    )
    .expect("write .git");
    let facts = scan_dir(&imposter);
    assert!(facts.git.repository);
    assert_eq!(ignored(&facts, "node_modules"), None);
    assert!(has_warning(&facts, "does not point back"));

    // Pointers that do not lead to a git directory are not repositories.
    for pointer in [
        "gitdir: /etc\n".to_string(),
        "gitdir: /does/not/exist\n".to_string(),
        "not a pointer\n".to_string(),
        "gitdir:\n".to_string(),
        format!("gitdir: {}\n", main.path().join(".git/objects").display()),
    ] {
        let repo = fixture(&[("package.json", "{}"), ("node_modules/.keep", "")]);
        fs::write(repo.path().join(".git"), &pointer).expect("write .git");
        let facts = scan_dir(&repo);
        assert!(!facts.git.repository, "{pointer:?}");
        assert_eq!(ignored(&facts, "node_modules"), None, "{pointer:?}");
    }
}

// -- 4. brace expansion -----------------------------------------------------------------

#[test]
fn finding_4_pathological_brace_patterns_are_dropped_not_recursed() {
    let hostile = "{}".repeat(8000);
    let nested = format!("{}a,b{}", "{".repeat(200), "}".repeat(200));
    let wide = "{a,b}".repeat(12);
    let manifest = serde_json::json!({
        "workspaces": [hostile, nested, wide, "{apps,libs}/*", "!{}"],
    });
    let repo = fixture(&[
        ("package.json", &manifest.to_string()),
        ("apps/web/package.json", "{}"),
        ("libs/ui/package.json", "{}"),
    ]);
    // Recursing once per group needs far more than this for 8000 groups.
    let root = repo.path().to_path_buf();
    let handle = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || scan(&root).expect("scan"))
        .expect("spawn");
    let facts = handle.join().expect("the scan overflowed its stack");

    let paths: Vec<&str> = facts.js[0]
        .packages
        .iter()
        .map(|p| p.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec![".", "apps/web", "libs/ui"],
        "valid patterns still work"
    );
    assert!(has_warning(&facts, "is too long"), "{:?}", facts.warnings);
    assert!(has_warning(&facts, "nests braces too deeply"));
    assert!(has_warning(&facts, "expands to too many alternatives"));
}

// -- 6. multi-line quoted values --------------------------------------------------------

#[test]
fn finding_6_text_inside_a_multiline_quoted_value_is_not_an_assignment() {
    let repo = fixture(&[(
        ".env",
        "SECRET=\"line1\nDB_SECRET_ABC=rest\nREDIS_URL=hidden\nline3\"\nPOSTGRES_USER=real\nKEY='a\nDB_HOST=b\n'\n",
    )]);
    let facts = scan_dir(&repo);

    assert!(!facts.env.has_key("DB_SECRET_ABC"), "{:?}", facts.env);
    assert!(!facts.env.has_key("REDIS_URL"));
    assert!(!facts.env.has_key("DB_HOST"));
    assert!(facts.env.has_key("POSTGRES_USER"));
    assert_eq!(
        facts.env.files[0].key_count, 3,
        "SECRET, POSTGRES_USER, KEY"
    );
}

#[test]
fn finding_6_an_unclosed_quote_ends_the_file_with_a_warning() {
    let repo = fixture(&[(
        ".env",
        "POSTGRES_USER=a\nSECRET=\"oops\nDB_HOST=b\nPORT=1\n",
    )]);
    let facts = scan_dir(&repo);

    assert!(facts.env.has_key("POSTGRES_USER"));
    assert!(!facts.env.has_key("DB_HOST"));
    assert!(has_warning(&facts, "never closed"));
}

// -- 7. work, not just results, is bounded ---------------------------------------------

/// 300 links to the root: `**` reaches ~90k directories at two levels, which
/// the old code collected in full before truncating to 300.
#[cfg(unix)]
fn looping_repo(workspaces: &str) -> TempDir {
    use std::os::unix::fs::symlink;
    let repo = fixture(&[(
        "package.json",
        &format!(r#"{{"workspaces":["{workspaces}"]}}"#),
    )]);
    for i in 0..300 {
        symlink(".", repo.path().join(format!("loop{i:03}"))).expect("symlink");
    }
    repo
}

#[cfg(unix)]
#[test]
fn finding_7_symlink_fan_out_in_a_workspace_glob_stops_at_the_result_cap() {
    let repo = looping_repo("**");
    let started = Instant::now();
    let facts = scan_within(repo.path(), Duration::from_secs(30));

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(
        has_warning(&facts, "matched too many directories"),
        "{:?}",
        facts.warnings
    );
    assert!(facts.js[0].packages.len() <= 301);
}

/// No match is ever found, so no result cap applies: only the work ceiling
/// stops a walk through ~90k aliases of the root.
#[cfg(unix)]
#[test]
fn finding_7_symlink_fan_out_in_a_workspace_glob_stops_at_the_work_ceiling() {
    let repo = looping_repo("*/*/missing");
    let started = Instant::now();
    let facts = scan_within(repo.path(), Duration::from_secs(30));

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(
        has_warning(&facts, "visited too many directories"),
        "{:?}",
        facts.warnings
    );
    // The rest of the scan still had budget to run.
    assert_eq!(facts.js[0].packages.len(), 1);
}

#[cfg(unix)]
#[test]
fn finding_7_rejected_directory_entries_count_against_the_entry_cap() {
    use std::os::unix::fs::symlink;
    let outside = tempfile::tempdir().expect("tempdir");
    let repo = fixture(&[("package.json", "{}")]);
    // Every one of these is dropped (it leaves the repository), so none would
    // be counted if only accepted entries were.
    for i in 0..6000 {
        symlink(outside.path(), repo.path().join(format!("link{i:04}"))).expect("symlink");
    }
    let started = Instant::now();
    let facts = scan_within(repo.path(), Duration::from_secs(30));

    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(has_warning(&facts, "only the first 4096 entries"));
    // And the warnings themselves are capped.
    assert!(facts.warnings.len() <= 201, "{}", facts.warnings.len());
    assert!(has_warning(&facts, "further warnings were not recorded"));
}

// -- 8. dotenv byte and key budgets -------------------------------------------------------

#[test]
fn finding_8_dotenv_streams_share_the_scan_byte_budget() {
    // 36 files just under the per-file cap exceed the 32 MiB scan budget.
    let line = format!("PAD={}\n", "x".repeat(990));
    let content = line.repeat(1000);
    let repo = tempfile::tempdir().expect("tempdir");
    for i in 0..36 {
        write(repo.path(), &format!(".env.part{i:02}"), &content);
    }
    write(
        repo.path(),
        ".env.part00",
        &format!("DATABASE_URL=1\n{content}"),
    );
    let facts = scan(repo.path()).expect("scan");

    assert!(facts.env.has_key("DATABASE_URL"), "the first file is read");
    assert!(
        has_warning(&facts, "budget"),
        "{:?}",
        facts.warnings.iter().take(5).collect::<Vec<_>>()
    );
}

#[test]
fn finding_8_a_file_of_many_names_is_capped_quickly() {
    let names: String = (0..60_000).map(|i| format!("NAME_{i}=1\n")).collect();
    let repo = fixture(&[(".env", &names)]);
    let started = Instant::now();
    let facts = scan_within(repo.path(), Duration::from_secs(30));

    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(facts.env.files[0].key_count, 5000);
    assert!(has_warning(&facts, "only the first 5000 variable names"));
}
