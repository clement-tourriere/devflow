//! devflow's verdict on LFS against `git check-attr`'s: the same files, the
//! same `$HOME`, and the two have to agree on whether `filter=lfs` applies.
use super::*;
use crate::worktree::gitoracle::{available, git, stdout};
use crate::worktree::testutil::{commit_all, signature};
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
enum Says {
    /// `filter=lfs` applies.
    Lfs,
    /// It does not.
    No,
    /// Git fails on the configuration, or devflow cannot tell.
    Fails,
}

/// A home directory (no symbolic links in its path, so that a `gitdir`
/// pattern means the same to both) with repositories in it.
struct Lab {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Lab {
    fn new() -> Lab {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        Lab { _dir: dir, home }
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        self.write_bytes(relative, text.as_bytes())
    }

    fn write_bytes(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// A file of attributes that sets the LFS filter.
    fn lfs_attributes(&self, relative: &str) -> PathBuf {
        self.write(relative, "*.psd filter=lfs diff=lfs merge=lfs -text\n")
    }

    /// A repository with one commit at `home/relative`.
    fn repo(&self, relative: &str) -> Repository {
        let root = self.home.join(relative);
        std::fs::create_dir_all(&root).unwrap();
        let repo = Repository::init(&root).unwrap();
        std::fs::write(root.join("README.md"), "x\n").unwrap();
        commit_all(&repo, "initial");
        let _ = signature();
        repo
    }

    fn append_local(&self, repo: &Repository, text: &str) {
        let path = repo.path().join("config");
        let mut config = std::fs::read(&path).unwrap();
        config.extend_from_slice(text.as_bytes());
        std::fs::write(path, config).unwrap();
    }

    fn git(&self, repo: &Repository) -> Says {
        let root = repo.workdir().unwrap();
        let out = git(
            &self.home,
            root,
            &["check-attr", "filter", "--", "probe.psd"],
        );
        if !out.status.success() {
            return Says::Fails;
        }
        if stdout(&out).contains("filter: lfs") {
            Says::Lfs
        } else {
            Says::No
        }
    }

    fn devflow(&self, repo: &Repository) -> Says {
        let env = rawconfig::Env {
            home: Some(self.home.clone()),
            xdg_config_home: None,
        };
        let mut roots: Vec<rawconfig::Root> = [".config/git/config", ".gitconfig"]
            .into_iter()
            .map(|name| rawconfig::Root {
                path: self.home.join(name),
                repository: false,
            })
            .collect();
        for name in ["config", "config.worktree"] {
            roots.push(rawconfig::Root {
                path: repo.path().join(name),
                repository: true,
            });
        }
        let commit = repo.head().unwrap().peel_to_commit().unwrap();
        let gathered = gather_from(repo, &roots, &env);
        match check_within(
            repo,
            &commit,
            repo.workdir().unwrap(),
            MAX_TREE_ENTRIES,
            &gathered,
        ) {
            Ok(_) => Says::No,
            Err(error) => {
                let message = error.downcast_ref::<Problem>().unwrap().message.clone();
                if message.starts_with("this repository uses Git LFS") {
                    Says::Lfs
                } else {
                    assert!(message.starts_with("devflow cannot"), "{message}");
                    Says::Fails
                }
            }
        }
    }

    /// Both answers, which must be `expected`.
    fn agree(&self, repo: &Repository, expected: Says) {
        let (git, devflow) = (self.git(repo), self.devflow(repo));
        assert_eq!(git, expected, "Git says {git:?}");
        assert_eq!(
            devflow, expected,
            "Git says {git:?}, devflow says {devflow:?}"
        );
    }
}

macro_rules! lab {
    () => {{
        if !available() {
            eprintln!("git is not installed: skipped");
            return;
        }
        Lab::new()
    }};
}

#[test]
fn the_default_user_attributes_file_applies() {
    let lab = lab!();
    lab.lfs_attributes(".config/git/attributes");
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::Lfs);
}

#[test]
fn a_setting_that_overrides_the_default_to_dev_null_leaves_no_lfs() {
    let lab = lab!();
    lab.lfs_attributes(".config/git/attributes");
    lab.lfs_attributes(".gitattributes");
    lab.write(".gitconfig", "[core]\n\tattributesFile = /dev/null\n");
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::No);

    // The repository's configuration overrides the user's.
    let lab = lab!();
    let attrs = lab.lfs_attributes("global-attrs");
    lab.write(
        ".gitconfig",
        &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
    );
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::Lfs);
    lab.append_local(&repo, "[core]\n\tattributesFile = /dev/null\n");
    lab.agree(&repo, Says::No);
}

#[test]
fn an_empty_value_means_no_file_and_not_the_default() {
    let lab = lab!();
    lab.lfs_attributes(".config/git/attributes");
    lab.write(".gitconfig", "[core]\n\tattributesFile =\n");
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::No);
}

#[test]
fn the_last_setting_wins_in_gits_order() {
    // ~/.gitconfig is read after the XDG file, the repository after both.
    let lab = lab!();
    let attrs = lab.lfs_attributes("attrs");
    lab.write(
        ".config/git/config",
        &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
    );
    lab.write(".gitconfig", "[core]\n\tattributesFile = /dev/null\n");
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::No);

    let lab = lab!();
    let attrs = lab.lfs_attributes("attrs");
    lab.write(
        ".config/git/config",
        "[core]\n\tattributesFile = /dev/null\n",
    );
    lab.write(
        ".gitconfig",
        &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
    );
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::Lfs);

    // An include takes the place of its directive: what comes after it wins.
    for (include_first, expected) in [(true, Says::Lfs), (false, Says::No)] {
        let lab = lab!();
        let attrs = lab.lfs_attributes("attrs");
        lab.write("inc", "[core]\n\tattributesFile = /dev/null\n");
        let include = format!("[include]\n\tpath = {}\n", lab.home.join("inc").display());
        let set = format!("[core]\n\tattributesFile = {}\n", attrs.display());
        let text = if include_first {
            format!("{include}{set}")
        } else {
            format!("{set}{include}")
        };
        lab.write(".gitconfig", &text);
        let repo = lab.repo("shop");
        lab.agree(&repo, expected);
    }
}

#[test]
fn an_overridden_setting_decides_nothing_even_when_it_cannot_be_used() {
    let lab = lab!();
    lab.lfs_attributes(".config/git/attributes");
    // Relative, `~user`, valueless: each unusable on its own.
    for (n, unusable) in [
        "attributesFile = relative/attrs",
        "attributesFile = ~someone/attrs",
        "attributesFile",
    ]
    .into_iter()
    .enumerate()
    {
        lab.write(".gitconfig", &format!("[core]\n\t{unusable}\n"));
        let repo = lab.repo(&format!("shop-{n}"));
        // Used as the effective value, devflow cannot tell (Git itself fails
        // on the valueless one and reads the others relative to where it
        // runs).
        assert_eq!(lab.devflow(&repo), Says::Fails, "{unusable}");
        // Overridden by the repository, it is not even looked at.
        lab.append_local(&repo, "[core]\n\tattributesFile = /dev/null\n");
        if unusable != "attributesFile = relative/attrs" {
            // Git itself dies on a valueless key or an unknown user's home
            // wherever it sits; what devflow decides is the effective value,
            // which is fine.
            assert_eq!(lab.devflow(&repo), Says::No);
        } else {
            lab.agree(&repo, Says::No);
        }
    }
    // The effective value is the one that is judged: the first being fine
    // does not rescue an unusable second.
    lab.write(".gitconfig", "[core]\n\tattributesFile = /dev/null\n");
    let repo = lab.repo("last");
    lab.append_local(&repo, "[core]\n\tattributesFile = relative\n");
    assert_eq!(lab.devflow(&repo), Says::Fails);
}

#[test]
fn a_valueless_setting_and_a_bad_line_are_what_git_refuses_too() {
    let lab = lab!();
    lab.write(".gitconfig", "[core]\n\tattributesFile\n");
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::Fails);

    let lab = lab!();
    lab.lfs_attributes(".config/git/attributes");
    lab.write(
        ".gitconfig",
        "[core]\n\tattributesFile = /dev/null\n[broken\n",
    );
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::Fails);
    // The message names the file and the line.
    let env = rawconfig::Env {
        home: Some(lab.home.clone()),
        xdg_config_home: None,
    };
    let roots = [rawconfig::Root {
        path: lab.home.join(".gitconfig"),
        repository: false,
    }];
    let found = rawconfig::walk(&repo, &roots, &env, &[]);
    let why = found.unknown.unwrap();
    assert!(
        why.contains("bad config line 3") && why.contains(".gitconfig"),
        "{why}"
    );
}

#[test]
fn a_byte_order_mark_hides_nothing() {
    let bom: &[u8] = b"\xef\xbb\xbf";
    let with_bom = |text: &str| [bom, text.as_bytes()].concat();

    // A global file that overrides the default.
    let lab = lab!();
    lab.lfs_attributes(".config/git/attributes");
    lab.write_bytes(
        ".gitconfig",
        &with_bom("[core]\n\tattributesFile = /dev/null\n"),
    );
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::No);

    // One that points at LFS rules.
    let lab = lab!();
    let attrs = lab.lfs_attributes("attrs");
    lab.write_bytes(
        ".gitconfig",
        &with_bom(&format!("[core]\n\tattributesFile = {}\n", attrs.display())),
    );
    let repo = lab.repo("shop");
    lab.agree(&repo, Says::Lfs);

    // A file reached through `hasconfig`, which libgit2 would not have read.
    let lab = lab!();
    let attrs = lab.lfs_attributes("attrs");
    lab.write_bytes(
        "work.inc",
        &with_bom(&format!("[core]\n\tattributesFile = {}\n", attrs.display())),
    );
    lab.write(
        ".gitconfig",
        &format!(
            "[includeIf \"hasconfig:remote.*.url:https://example.com/**\"]\n\tpath = {}\n",
            lab.home.join("work.inc").display()
        ),
    );
    let repo = lab.repo("shop");
    repo.remote("origin", "https://example.com/team/app.git")
        .unwrap();
    lab.agree(&repo, Says::Lfs);

    // The repository's own file.
    let lab = lab!();
    lab.lfs_attributes(".config/git/attributes");
    let repo = lab.repo("shop");
    let local = repo.path().join("config");
    let text = std::fs::read(&local).unwrap();
    std::fs::write(
        &local,
        [
            bom,
            text.as_slice(),
            b"[core]\n\tattributesFile = /dev/null\n",
        ]
        .concat(),
    )
    .unwrap();
    lab.agree(&repo, Says::No);

    // A mark that is not at the start is a syntax error for Git, and so for us.
    let lab = lab!();
    let repo = lab.repo("shop");
    lab.append_local(&repo, "\u{feff}[core]\n\tattributesFile = /dev/null\n");
    lab.agree(&repo, Says::Fails);
}

#[test]
fn a_hasconfig_include_reaches_a_nested_gitdir_include() {
    let lab = lab!();
    let attrs = lab.lfs_attributes("attrs");
    lab.write(
        ".gitconfig",
        "[includeIf \"hasconfig:remote.*.url:https://example.com/**\"]\n\tpath = work.inc\n",
    );
    let repo = lab.repo("shop");
    repo.remote("origin", "https://example.com/team/app.git")
        .unwrap();
    // Relative to the file that holds it, and evaluated for this repository.
    lab.write(
        "work.inc",
        &format!(
            "[includeIf \"gitdir:{}/\"]\n\tpath = deep.inc\n\
             [includeIf \"gitdir:/somewhere/else/\"]\n\tpath = wrong.inc\n",
            lab.home.join("shop").display()
        ),
    );
    lab.write(
        "deep.inc",
        &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
    );
    lab.write("wrong.inc", "[core]\n\tattributesFile = /dev/null\n");
    lab.agree(&repo, Says::Lfs);

    // Another repository's gitdir condition does not apply.
    let other = lab.repo("other");
    other
        .remote("origin", "https://example.com/team/app.git")
        .unwrap();
    lab.write(
        "work.inc",
        "[includeIf \"gitdir:/not/this/one/\"]\n\tpath = deep.inc\n",
    );
    lab.agree(&other, Says::No);
}

#[test]
fn a_bracket_in_a_gitdir_condition_is_evaluated_not_refused() {
    // The global condition that used to block every creation.
    for (dir, expected) in [
        ("work/proj", Says::Lfs),
        ("Work/proj", Says::Lfs),
        ("worker/proj", Says::No),
        ("nested/work/proj", Says::No),
        ("other/proj", Says::No),
        ("elsewhere", Says::No),
    ] {
        // (A fresh home each time: the file system may not tell `work` from
        // `Work`.)
        let lab = lab!();
        let attrs = lab.lfs_attributes("attrs");
        lab.write(
            "lfs.inc",
            &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
        );
        lab.write(
            ".gitconfig",
            &format!(
                "[includeIf \"gitdir:~/[Ww]ork/\"]\n\tpath = {}\n",
                lab.home.join("lfs.inc").display()
            ),
        );
        let repo = lab.repo(dir);
        lab.agree(&repo, expected);
    }
}

#[test]
fn onbranch_and_gitdir_conditions_agree_with_git() {
    let lab = lab!();
    let attrs = lab.lfs_attributes("attrs");
    lab.write(
        "lfs.inc",
        &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
    );
    let inc = lab.home.join("lfs.inc");
    let repo = lab.repo("shop");
    let branch = repo.head().unwrap().shorthand().unwrap().to_string();
    let root = lab.home.join("shop");
    for (n, (condition, applies)) in [
        (format!("onbranch:{branch}"), true),
        ("onbranch:*".to_string(), true),
        ("onbranch:release/**".to_string(), false),
        (
            "onbranch:[a-z]*".to_string(),
            branch.starts_with(|c: char| c.is_ascii_lowercase()),
        ),
        (format!("gitdir:{}/", root.display()), true),
        ("gitdir:**/shop/.git".to_string(), true),
        ("gitdir:shop/.git".to_string(), true),
        ("gitdir/i:**/SHOP/.GIT".to_string(), true),
        ("gitdir:**/s[h]op/".to_string(), true),
        ("gitdir:**/s[!h]op/".to_string(), false),
        ("gitdir:**/other/.git".to_string(), false),
        ("gitdir:~/nowhere/".to_string(), false),
    ]
    .into_iter()
    .enumerate()
    {
        lab.write(
            ".gitconfig",
            &format!("[includeIf \"{condition}\"]\n\tpath = {}\n", inc.display()),
        );
        let expected = if applies { Says::Lfs } else { Says::No };
        let (git, devflow) = (lab.git(&repo), lab.devflow(&repo));
        assert_eq!(git, expected, "#{n} {condition}: Git says {git:?}");
        assert_eq!(
            devflow, expected,
            "#{n} {condition}: devflow says {devflow:?}"
        );
    }
}
