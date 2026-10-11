//! Repository detection: a pure, read-only scan that reports *facts*.
//!
//! [`scan`] looks at a checkout and returns a serializable [`Facts`]: package
//! managers, frameworks, migration tools, compose services, `.env*` key names,
//! dev processes, Git-ignored directories worth cloning, and the devflow,
//! pitchfork and mise configuration already present. It makes no decisions;
//! a later layer turns facts into a proposed configuration.
//!
//! # Threat model
//!
//! Detection defends against hostile repository **content**: committed files,
//! committed symlinks and hard-link-like aliases, crafted ignore files and git
//! indexes, and huge or malformed files. Whatever such content does, the scan
//! must stay inside the checkout, finish in bounded time and memory, never run
//! project code, and never put `.env` or `.devflow/env` values, or any other
//! credential, into its output.
//!
//! It does **not** defend against a concurrent local process running as the
//! same user with write access to the checkout: that process can read the
//! secrets directly, so there is nothing to protect them from. The scan still
//! applies cheap mitigations against such races where they cost almost
//! nothing (a name checked is re-verified when it is opened, the opened
//! descriptor is compared with known protected files, a swapped file or
//! directory is refused), but they are best effort, not a guarantee, and a
//! race can at worst make the scan report less.
//!
//! # Guarantees
//!
//! - **Never executes project code.** Manifests are parsed as data; scripts,
//!   build files and configs written in code (`drizzle.config.ts`,
//!   `prisma.config.ts`) are only searched for literal strings.
//! - **Never reads `.env*` values.** Files are scanned for variable names
//!   only; no value is stored or returned, and quoted values spanning several
//!   lines are skipped as a whole, whether or not the name before the `=` is
//!   usable. Where dotenv parsers disagree about quoting (escapes in single
//!   quotes, backslash pairs, what counts as blank space before a quote) the
//!   reading that keeps more text inside the value wins. Dotenv files, `.devflow/`, `.git/` and credential stores are
//!   opened by that keys-only reader alone: a symlink named `.nvmrc` that
//!   leads to `.env` is refused, not read as a version, and so is any file
//!   with several hard links (the other name may be a dotenv file anywhere).
//!   Version pins and `.tool-versions` entries are further limited to
//!   version-like words, so a value that does get through is not a secret in
//!   disguise.
//! - **Stays inside the repository.** All access goes through one `cap-std`
//!   directory handle: each path is resolved from the root descriptor one
//!   component at a time and symlinks are followed only while they stay
//!   beneath it (a file or directory swapped for a link to `/etc`, or for a
//!   FIFO, mid-scan is refused or rejected). Files are opened `O_NONBLOCK` and
//!   the *opened* descriptor is validated (regular file, size cap, link
//!   count, identity). A path whose name was inspected before it was opened is
//!   opened again component by component without following links, each step
//!   confirmed with `lstat`, so a plain file swapped for a symlink in between
//!   is refused.
//! - **No credentials in facts.** No command text from the repository is
//!   reported: a Procfile process is shown as `<file>: <process>` plus a
//!   pointer to its line, and every other process is an exact argument vector
//!   ([`ProcessLaunch`]) with script and task names as single arguments, never
//!   text spliced into a shell command.
//! - **Git without libgit2.** Ignore rules come from `.gitignore` files and
//!   `$GIT_DIR/info/exclude` read through that same reader and matched
//!   in-process; the index is parsed from bytes the reader returned. Git
//!   config (`include`, `core.excludesFile`) and global excludes are not
//!   read; ignore status is `None` (unknown) whenever an input cannot be used
//!   safely or completely, which includes a split or sparse index and an
//!   index whose prefix-compressed paths expand beyond the scan's limits or
//!   whose checksum matches no hash size (an unchecked trailer of zeros is
//!   accepted only when every reading of it agrees).
//!   The one exception to "inside the repository" is a `.git` *file* of a
//!   linked worktree: its git directory is opened as a second confined
//!   handle, only after checking that it is a git directory that names a
//!   common directory and points back at this checkout's `.git` file, and only
//!   `HEAD`, `commondir`, `gitdir`, `info/exclude` and `index` are read from
//!   it. A submodule's or `--separate-git-dir` checkout's git directory has
//!   no such back pointer the scan can read, so its ignore status is unknown.
//! - **Bounded work, not just bounded results.** Files over 1 MiB are
//!   skipped, a scan reads at most 32 MiB (the expanded paths of a git index
//!   count against that budget, at most 4096 bytes each) and makes at most
//!   250,000 filesystem visits (every directory entry examined, rejected or
//!   not, every lookup attempted, found or not, every path resolved and every
//!   file opened is charged), directories list at most 4096 entries, at most
//!   80 processes are collected (a source is not read once the list is full),
//!   project discovery stops at 300 directories,
//!   workspace globs expand two levels with an iterative, length- and
//!   depth-limited brace expansion, and YAML parsing has alias-expansion
//!   limits. When a budget runs out the scan stops that work and says so in
//!   [`Facts::warnings`].
//! - **Fast.** A typical repository costs a few dozen small reads: a few
//!   milliseconds in a release build, far below the 100 ms budget.
//!
//! # Project directories
//!
//! Besides the root, the scan inspects JavaScript workspace members, Cargo
//! workspace members, directories directly under the root, and directories
//! one level below `apps/`, `packages/`, `services/`, `libs/`, `modules/`,
//! `projects/`, `crates/` and `workers/`; only directories holding a
//! recognized manifest count. Facts carry the `path` of the directory they
//! belong to (`"."` for the root).
//!
//! # Evidence
//!
//! Facts list the files, and after a colon the keys, that support them
//! (`bun.lock`, `package.json:dependencies.next`, `compose.yaml:services.db.image`).
//!
//! A failure to read or parse one file never fails the scan; it is reported
//! in [`Facts::warnings`].
use anyhow::Result;
use std::path::Path;

mod artifacts;
mod compose;
mod drivers;
mod env;
mod facts;
mod frameworks;
mod globs;
mod js;
mod manifest;
mod migrations;
mod processes;
mod python;
mod repo;
pub(crate) mod toolchain;
mod wiring;
mod yaml;

#[cfg(test)]
mod tests;

pub use facts::*;
pub(crate) use wiring::is_devflow_env_path;

/// Scan the checkout at `root` and report what it contains.
///
/// `root` must be an existing directory (the top of a repository or
/// worktree). Fails only when `root` itself cannot be used; unreadable or
/// malformed files inside it become [`Facts::warnings`].
pub fn scan(root: &Path) -> Result<Facts> {
    let repo = repo::Repo::open(root)?;
    let discovery = manifest::discover(&repo);

    let toolchain = toolchain::detect(&repo);
    let js = js::detect(&repo, &discovery);
    let python = python::detect(&discovery);
    let frameworks = frameworks::detect(&repo, &discovery);
    let migrations = migrations::detect(&repo, &discovery, &frameworks);
    let drivers = drivers::detect(&discovery);
    let compose = compose::detect(&repo);
    let env = env::detect(&repo, &discovery);
    let wiring = wiring::detect(&repo, &toolchain);
    let processes = processes::detect(
        &repo,
        &discovery,
        &js,
        &python,
        &frameworks,
        &toolchain,
        &wiring,
    );
    let artifacts = artifacts::detect(&repo, &discovery, &js, &frameworks, &migrations);

    Ok(Facts {
        root: repo.root().to_path_buf(),
        git: GitFacts {
            repository: repo.is_git(),
        },
        toolchain,
        js,
        python,
        frameworks,
        migrations,
        drivers,
        compose,
        env,
        processes,
        artifacts,
        wiring,
        warnings: repo.take_warnings(),
    })
}

/// The mise configuration files at the top of `root`, lowest precedence first
/// (the order mise applies them): every location mise reads, `conf.d` and the
/// environment-specific and `.local` files included, found by the same
/// discovery [`scan`] uses for [`Facts::toolchain`]. The root must be an
/// existing directory.
pub fn mise_layers(root: &Path) -> Result<Vec<String>> {
    let repo = repo::Repo::open(root)?;
    Ok(toolchain::mise_files(&repo)
        .into_iter()
        .map(|(path, _)| path)
        .collect())
}

/// Whether a path relative to the top of a checkout is a mise configuration
/// file (see [`mise_layers`]).
pub(crate) fn is_mise_layer(path: &str) -> bool {
    toolchain::is_layer(path)
}

/// Append `item` unless the list already has it.
fn push_unique(list: &mut Vec<String>, item: impl Into<String>) {
    let item = item.into();
    if !list.contains(&item) {
        list.push(item);
    }
}
