//! The scan must stay inside the repository, never block, never read values,
//! and bound the work an adversarial repository can cause.
use super::*;
use std::time::{Duration, Instant};

#[cfg(unix)]
mod symlinks {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn files_linked_from_outside_the_repository_are_ignored() {
        let outside = fixture(&[
            (
                "package.json",
                r#"{"packageManager":"pnpm@9.0.0","dependencies":{"next":"15"}}"#,
            ),
            (".env", "DATABASE_URL=VALUE_MUST_NOT_LEAK\n"),
            ("compose.yaml", "services:\n  db:\n    image: postgres:16\n"),
            ("mise.toml", "[tasks.pwned]\nrun = 'true'\n"),
            ("secret/.keep", ""),
        ]);
        let repo = fixture(&[("README.md", "hi")]);
        for name in ["package.json", ".env", "compose.yaml", "mise.toml"] {
            symlink(outside.path().join(name), repo.path().join(name)).expect("symlink");
        }
        symlink(
            outside.path().join("secret"),
            repo.path().join("node_modules"),
        )
        .expect("symlink");
        let facts = scan_dir(&repo);

        assert!(facts.js.is_empty(), "{:?}", facts.js);
        assert!(facts.frameworks.is_empty());
        assert!(facts.compose.is_none());
        assert!(facts.env.files.is_empty());
        assert!(facts.toolchain.mise.is_none());
        assert!(facts.artifacts.is_empty());
        assert!(
            facts
                .warnings
                .iter()
                .any(|w| w.message.contains("outside the repository")),
            "{:?}",
            facts.warnings
        );
        let json = serde_json::to_string(&facts).expect("json");
        assert!(!json.contains("VALUE_MUST_NOT_LEAK"));
    }

    #[test]
    fn a_directory_symlink_cannot_pull_in_a_foreign_project() {
        let outside = fixture(&[(
            "package.json",
            r#"{"dependencies":{"next":"15"},"scripts":{"dev":"next dev"}}"#,
        )]);
        let repo = fixture(&[("package.json", r#"{"workspaces":["apps/*"]}"#)]);
        mkdir(repo.path(), "apps");
        symlink(outside.path(), repo.path().join("apps/evil")).expect("symlink");
        symlink(outside.path(), repo.path().join("web")).expect("symlink");
        let facts = scan_dir(&repo);

        assert!(!facts.has_framework(Framework::NextJs));
        assert_eq!(facts.js.len(), 1);
        assert_eq!(facts.js[0].packages.len(), 1, "only the root package");
        assert!(facts.processes.is_empty());
    }

    #[test]
    fn links_that_stay_inside_the_repository_are_followed() {
        let repo = fixture(&[("real/package.json", r#"{"dependencies":{"next":"15"}}"#)]);
        symlink(repo.path().join("real"), repo.path().join("web")).expect("symlink");
        let facts = scan_dir(&repo);

        assert!(
            facts
                .frameworks
                .iter()
                .any(|f| f.framework == Framework::NextJs),
            "{:?}",
            facts.frameworks
        );
    }

    #[test]
    fn symlinked_artifacts_are_not_reported() {
        let repo = fixture(&[
            ("package.json", "{}"),
            ("shared/modules/.keep", ""),
            (".gitignore", "node_modules\n"),
        ]);
        git_init(repo.path());
        symlink(
            repo.path().join("shared/modules"),
            repo.path().join("node_modules"),
        )
        .expect("symlink");
        let facts = scan_dir(&repo);

        assert!(artifact(&facts, "node_modules").is_none());
    }

    #[test]
    fn a_dangling_or_looping_link_is_harmless() {
        let repo = fixture(&[("README.md", "hi")]);
        symlink("missing-target", repo.path().join("package.json")).expect("symlink");
        symlink("loop-b", repo.path().join("loop-a")).expect("symlink");
        symlink("loop-a", repo.path().join("loop-b")).expect("symlink");
        symlink("loop-a", repo.path().join(".env")).expect("symlink");
        let facts = scan_dir(&repo);

        assert!(facts.js.is_empty());
        assert!(facts.env.files.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn a_fifo_named_like_a_manifest_does_not_block_the_scan() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let repo = fixture(&[("README.md", "hi")]);
    for name in ["package.json", ".env", "compose.yaml", "mise.toml"] {
        let path = CString::new(repo.path().join(name).as_os_str().as_bytes()).expect("path");
        // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
        let rc = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed");
    }
    let root = repo.path().to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(scan(&root));
    });
    let facts = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the scan blocked on a FIFO")
        .expect("scan");
    assert!(facts.js.is_empty());
    assert!(facts.env.files.is_empty());
    assert!(facts.compose.is_none());
}

#[test]
fn huge_files_are_skipped_with_a_warning() {
    let padding = "x".repeat(1024 * 1024 + 16);
    let huge_json = format!(r#"{{"name":"big","dependencies":{{"next":"15"}},"pad":"{padding}"}}"#);
    let huge_env = format!("DATABASE_URL=1\nPAD={padding}\n");
    let huge_compose = format!("services:\n  db:\n    image: postgres:16\n# {padding}\n");
    let repo = fixture(&[
        ("package.json", &huge_json),
        (".env", &huge_env),
        ("compose.yaml", &huge_compose),
        ("mise.toml", &format!("# {padding}\n[tasks.dev]\nrun='x'\n")),
        // A small neighbour is still read.
        ("apps/web/package.json", r#"{"dependencies":{"next":"15"}}"#),
    ]);
    let facts = scan_dir(&repo);

    assert!(facts.compose.is_none());
    assert!(facts.env.files.is_empty());
    assert!(!facts.env.has_database_keys);
    assert!(
        facts
            .toolchain
            .mise
            .as_ref()
            .is_none_or(|m| m.tasks.is_empty())
    );
    for file in ["package.json", ".env", "compose.yaml", "mise.toml"] {
        assert!(
            facts
                .warnings
                .iter()
                .any(|w| w.path == file && w.message.contains("larger than 1024 KiB")),
            "no size warning for {file}: {:?}",
            facts.warnings
        );
    }
    assert!(
        facts
            .frameworks
            .iter()
            .any(|f| f.framework == Framework::NextJs && f.path == "apps/web")
    );
    assert!(
        !facts
            .frameworks
            .iter()
            .any(|f| f.framework == Framework::NextJs && f.path == "."),
        "the oversized root manifest must not be partially used"
    );
}

#[test]
fn a_file_just_under_the_cap_is_read() {
    let padding = "x".repeat(1024 * 1024 - 200);
    let json = format!(r#"{{"dependencies":{{"next":"15"}},"pad":"{padding}"}}"#);
    let repo = fixture(&[("package.json", &json)]);
    let facts = scan_dir(&repo);

    assert!(facts.has_framework(Framework::NextJs));
    assert!(facts.warnings.is_empty());
}

#[test]
fn workspace_globs_cannot_leave_the_repository() {
    let outside = fixture(&[("evil/package.json", r#"{"dependencies":{"next":"15"}}"#)]);
    let repo = fixture(&[(
        "package.json",
        &format!(
            r#"{{"workspaces":["../*/evil","{}/*","./../x","apps/../../y"]}}"#,
            outside.path().display()
        ),
    )]);
    let facts = scan_dir(&repo);

    assert!(!facts.has_framework(Framework::NextJs));
    assert_eq!(facts.js[0].packages.len(), 1);
    assert!(
        facts
            .warnings
            .iter()
            .any(|w| w.message.contains("outside the repository")),
        "{:?}",
        facts.warnings
    );
}

#[test]
fn prisma_output_outside_the_repository_is_dropped() {
    let repo = fixture(&[
        ("package.json", r#"{"devDependencies":{"prisma":"6"}}"#),
        (
            "prisma/schema.prisma",
            "generator a {\n  provider = \"x\"\n  output = \"../../elsewhere\"\n}\ngenerator b {\n  provider = \"x\"\n  output = \"/etc/passwd\"\n}\ngenerator c {\n  provider = \"x\"\n  output = \"../ok/client\"\n}\n",
        ),
    ]);
    let facts = scan_dir(&repo);

    let prisma = migration(&facts, MigrationTool::Prisma).expect("prisma");
    assert_eq!(prisma.generated_outputs, vec!["ok/client"]);
}

#[test]
fn env_values_are_never_returned_even_in_odd_lines() {
    let repo = fixture(&[(
        ".env",
        "A=VALUE_MUST_NOT_LEAK\nexport DATABASE_URL = \"VALUE_MUST_NOT_LEAK\"\nVALUE_MUST_NOT_LEAK_KEY_WITHOUT_EQUALS\nREDIS_URL='VALUE_MUST_NOT_LEAK'\n# SECRET=VALUE_MUST_NOT_LEAK\nMULTI=\"first\nVALUE_MUST_NOT_LEAK=still_a_value_line\"\n",
    )]);
    let facts = scan_dir(&repo);

    assert!(facts.env.has_key("DATABASE_URL"));
    assert!(facts.env.has_key("REDIS_URL"));
    let json = serde_json::to_string(&facts).expect("json");
    // Names that merely *look* like keys on continuation lines are the worst case; values never appear.
    assert!(!json.contains("still_a_value_line"));
    assert!(!json.contains("first"));
    assert_eq!(facts.env.files[0].path, ".env");
}

#[test]
fn compose_environment_values_are_not_retained() {
    let repo = fixture(&[(
        "compose.yaml",
        "services:\n  db:\n    image: postgres:16\n    environment:\n      - POSTGRES_PASSWORD=VALUE_MUST_NOT_LEAK\n    env_file: [.env.secret]\n",
    )]);
    let facts = scan_dir(&repo);

    assert_eq!(
        service(&facts, "db").environment_keys,
        vec!["POSTGRES_PASSWORD"]
    );
    assert!(
        !serde_json::to_string(&facts)
            .expect("json")
            .contains("VALUE_MUST_NOT_LEAK")
    );
}

#[test]
fn yaml_alias_bombs_are_bounded() {
    let mut yaml = String::from(
        "x-a: &a [\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\"]\n",
    );
    let mut previous = 'a';
    for next in 'b'..='k' {
        let refs = vec![format!("*{previous}"); 9].join(",");
        yaml.push_str(&format!("x-{next}: &{next} [{refs}]\n"));
        previous = next;
    }
    yaml.push_str("services:\n  db:\n    image: postgres:16\n");
    let repo = fixture(&[("compose.yaml", &yaml), ("pnpm-workspace.yaml", &yaml)]);

    let started = Instant::now();
    let facts = scan_dir(&repo);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "alias expansion was not bounded: {:?}",
        started.elapsed()
    );
    // Either the document is rejected or its services are read; never a hang or panic.
    assert!(facts.compose.as_ref().is_none_or(|c| c.services.len() <= 1));
}

#[test]
fn malformed_files_become_warnings_not_errors() {
    let repo = fixture(&[
        ("package.json", "{ not json"),
        ("compose.yaml", "services: [unclosed"),
        ("mise.toml", "[tasks\n"),
        ("pyproject.toml", "= nope"),
        ("Cargo.toml", "[package"),
        ("pitchfork.toml", "[[[["),
        ("composer.json", "<<<"),
    ]);
    let facts = scan_dir(&repo);

    for file in [
        "package.json",
        "compose.yaml",
        "mise.toml",
        "pyproject.toml",
        "Cargo.toml",
        "pitchfork.toml",
        "composer.json",
    ] {
        assert!(
            facts
                .warnings
                .iter()
                .any(|w| w.path == file && w.message.starts_with("invalid")),
            "no warning for {file}: {:?}",
            facts.warnings
        );
    }
    // Non-UTF-8 content does not fail either.
    let repo = tempfile::tempdir().expect("tempdir");
    std::fs::write(repo.path().join(".env"), b"DATABASE_URL=\xff\xfe\n").expect("write");
    std::fs::write(repo.path().join("package.json"), b"\xff\xfe").expect("write");
    let facts = scan(repo.path()).expect("scan");
    assert!(facts.env.has_key("DATABASE_URL"));
}

#[test]
fn a_large_monorepo_scans_quickly() {
    let mut files: Vec<(String, String)> = vec![
        (
            "package.json".into(),
            r#"{"name":"root","packageManager":"pnpm@9"}"#.into(),
        ),
        (
            "pnpm-workspace.yaml".into(),
            "packages:\n  - apps/*\n  - packages/*\n".into(),
        ),
        ("pnpm-lock.yaml".into(), String::new()),
    ];
    for i in 0..60 {
        for group in ["apps", "packages"] {
            files.push((
                format!("{group}/p{i}/package.json"),
                format!(r#"{{"name":"p{i}","scripts":{{"dev":"x","build":"y"}},"dependencies":{{"next":"15","pg":"8"}}}}"#),
            ));
        }
    }
    let repo = tempfile::tempdir().expect("tempdir");
    for (path, content) in &files {
        write(repo.path(), path, content);
    }
    git_init(repo.path());

    let started = Instant::now();
    let facts = scan(repo.path()).expect("scan");
    let elapsed = started.elapsed();
    assert_eq!(facts.js[0].packages.len(), 121);
    // The target is well under 100 ms on a typical repository; the bound here is
    // deliberately loose so a slow CI runner does not make the test flaky.
    assert!(elapsed < Duration::from_secs(3), "scan took {elapsed:?}");
}
