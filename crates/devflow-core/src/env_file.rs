//! The generated `.devflow/env` file in each worktree. devflow owns the whole
//! `.devflow/` directory and keeps it out of Git with its own `.gitignore`.
//!
//! Besides the variables devflow generates, the file holds the project's
//! `[env]` recipes and presets, rendered by [`render`].
use crate::config::{EnvPreset, EnvSection};
use crate::conn;
use crate::diag::key_label;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

mod commands;
mod mise;
pub use commands::{
    HookMise, MiseUse, Unfaithful, classify_hooks, command_is_wholly_devflow_exec,
    command_uses_mise, mise_subcommands, mise_uses,
};
pub(crate) use mise::shell_word;
pub use mise::{
    DEFAULT_MISE_TIMEOUT, MiseCommand, MiseDelivery, check_command_env, check_mise_delivery,
    check_mise_delivery_with, check_mise_trust, mise_configs,
};

pub const DIR: &str = ".devflow";
pub const FILE: &str = "env";

/// Variables devflow generates besides the `PG*` ones (which depend on the
/// connection URL). Recipes may reference these and cannot set them.
pub const GENERATED: [&str; 8] = [
    "DATABASE_URL",
    "DEVFLOW_PROJECT",
    "DEVFLOW_PROJECT_ROOT",
    "DEVFLOW_WORKSPACE",
    "DEVFLOW_WORKSPACE_PATH",
    "DEVFLOW_SOURCE_PATH",
    "DEVFLOW_WORKSPACE_ID",
    "COMPOSE_PROJECT_NAME",
];

/// Whether devflow generates `name` for some workspace (so a recipe may
/// reference it; whether this workspace has a value is another matter).
pub fn is_generated(name: &str) -> bool {
    GENERATED.contains(&name) || conn::pg_env_names().any(|pg| pg == name)
}

/// Names a recipe cannot set: everything devflow generates, `DEVFLOW_*`,
/// `PG*` and the test and shadow database URLs it will generate.
pub fn is_reserved(name: &str) -> bool {
    name.starts_with("DEVFLOW_")
        || name.starts_with("PG")
        || GENERATED.contains(&name)
        || matches!(name, "TEST_DATABASE_URL" | "SHADOW_DATABASE_URL")
}

/// A piece of a recipe value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    /// `${NAME}`
    Var(String),
}

/// Split a recipe value into text and `${NAME}` references. Only that exact
/// form is a reference: no defaults (`${A:-b}`), no nesting, no command
/// substitution. A lone `$` is text, and `$${` is a literal `${`.
///
/// The error is a fixed text. It never quotes the template, which may hold a
/// credential (`${TOKEN:-s3cr3t}` is a reference with a default value).
pub fn parse_template(template: &str) -> std::result::Result<Vec<Segment>, &'static str> {
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        text.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(after) = rest.strip_prefix("$${") {
            text.push_str("${");
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                return Err("`${` is never closed; write `$${` for a literal `${`");
            };
            let name = &after[..end];
            if !crate::config::valid_env_name(name) {
                return Err(
                    "unsupported reference syntax: only `${NAME}` with a plain variable name is \
                     allowed (no defaults, no nesting); write `$${` for a literal `${`",
                );
            }
            if !text.is_empty() {
                segments.push(Segment::Text(std::mem::take(&mut text)));
            }
            segments.push(Segment::Var(name.to_string()));
            rest = &after[end + 1..];
        } else {
            text.push('$');
            rest = &rest[1..];
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        segments.push(Segment::Text(text));
    }
    Ok(segments)
}

/// What [`render`] produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    /// The recipe and preset variables to add to the generated ones.
    pub vars: Vec<(String, String)>,
    /// Names among `vars` whose value comes from a credential.
    pub secret_keys: Vec<String>,
    /// The values of those variables, to be masked whole wherever hook output
    /// is shown or logged.
    pub secrets: Vec<String>,
    /// Recipes and presets left out, and why.
    pub warnings: Vec<String>,
}

/// Render the `[env]` section against the variables devflow generated for
/// this workspace (`generated`) and the workspace's PostgreSQL URL
/// (`postgres_url`, the binding's; `None` without one).
///
/// References are substituted in one pass from `generated` alone. A value is
/// secret when it was built from a generated variable that carries a
/// credential (`DATABASE_URL`, `PGPASSWORD`, …); presets mark their password
/// and URL fields the same way. A recipe whose reference has no value in this
/// workspace (no database, say) is left out with a warning, and so are the
/// presets of a workspace without a PostgreSQL binding.
pub fn render(
    section: &EnvSection,
    generated: &[(String, String)],
    postgres_url: Option<&str>,
) -> Rendered {
    let mut out = Rendered::default();
    let lookup: BTreeMap<&str, &str> = generated
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let secret_sources: BTreeSet<&str> = generated
        .iter()
        .filter(|(key, value)| !crate::hooks::secret_values(key, value).is_empty())
        .map(|(key, _)| key.as_str())
        .collect();
    let mut taken: BTreeSet<String> = generated.iter().map(|(key, _)| key.clone()).collect();
    let mut add = |out: &mut Rendered, key: &str, value: String, secret: bool| {
        if !taken.insert(key.to_string()) {
            out.warnings.push(format!(
                "env.{} was left out: devflow already generates a variable of that name",
                key_label(key)
            ));
            return;
        }
        if secret {
            out.secret_keys.push(key.to_string());
            out.secrets.push(value.clone());
        }
        out.vars.push((key.to_string(), value));
    };

    if !section.presets.is_empty() {
        let names = section
            .presets
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        match postgres_url.map(conn::Params::parse) {
            None => out.warnings.push(format!(
                "env presets ({names}) were left out: this workspace has no postgres binding"
            )),
            Some(Err(error)) => out.warnings.push(format!(
                "env presets ({names}) were left out: the database URL cannot be read ({error})"
            )),
            Some(Ok(params)) => {
                for preset in &section.presets {
                    match preset_vars(*preset, postgres_url.unwrap_or_default(), &params) {
                        Ok(vars) => {
                            for (key, value, secret) in vars {
                                add(&mut out, key, value, secret);
                            }
                        }
                        Err(why) => out
                            .warnings
                            .push(format!("env preset \"{preset}\" was left out: {why}")),
                    }
                }
            }
        }
    }

    for (key, template) in &section.vars {
        let Ok(segments) = parse_template(template) else {
            // Configuration validation refuses these before they get here.
            out.warnings.push(format!(
                "env.{} was left out: its value is not valid",
                key_label(key)
            ));
            continue;
        };
        let mut value = String::new();
        let mut secret = false;
        let mut missing = None;
        for segment in &segments {
            match segment {
                Segment::Text(text) => value.push_str(text),
                Segment::Var(name) => match lookup.get(name.as_str()) {
                    Some(found) => {
                        value.push_str(found);
                        secret |= secret_sources.contains(name.as_str());
                    }
                    None => {
                        missing = Some(name.as_str());
                        break;
                    }
                },
            }
        }
        match missing {
            Some(name) => out.warnings.push(format!(
                "env.{} was left out: ${{{}}} has no value in this workspace",
                key_label(key),
                key_label(name)
            )),
            None => add(&mut out, key, value, secret),
        }
    }
    out.secrets.sort();
    out.secrets.dedup();
    out
}

/// The variables of `preset` for the connection `params` (read from `url`).
/// Each is `(name, value, carries a credential)`. `Err` says what the
/// connection lacks.
fn preset_vars(
    preset: EnvPreset,
    url: &str,
    params: &conn::Params,
) -> std::result::Result<Vec<(&'static str, String, bool)>, String> {
    let need = |keyword: &str| {
        params
            .get(keyword)
            .ok_or_else(|| format!("the database URL has no {keyword}"))
    };
    let (host, user, database) = (need("host")?, need("user")?, need("dbname")?);
    let port = params.get("port").unwrap_or("5432");
    let password = params.password();
    // `sslmode` is the one connection option the URL forms below can carry.
    let ssl = params
        .get("sslmode")
        .map(|mode| format!("?sslmode={}", conn::url_component(mode)))
        .unwrap_or_default();
    let mut vars: Vec<(&'static str, String, bool)> = Vec::new();
    let mut push = |key: &'static str, value: String, secret: bool| vars.push((key, value, secret));
    match preset {
        EnvPreset::Prisma => push("DIRECT_URL", url.trim().to_string(), password.is_some()),
        EnvPreset::Laravel => {
            push("DB_CONNECTION", "pgsql".into(), false);
            push("DB_HOST", host.into(), false);
            push("DB_PORT", port.into(), false);
            push("DB_DATABASE", database.into(), false);
            push("DB_USERNAME", user.into(), false);
            if let Some(password) = password {
                push("DB_PASSWORD", password.into(), true);
            }
        }
        EnvPreset::Spring => {
            push(
                "SPRING_DATASOURCE_URL",
                format!(
                    "jdbc:postgresql://{}:{port}/{}{ssl}",
                    conn::url_host(host),
                    conn::url_component(database)
                ),
                false,
            );
            push("SPRING_DATASOURCE_USERNAME", user.into(), false);
            if let Some(password) = password {
                push("SPRING_DATASOURCE_PASSWORD", password.into(), true);
            }
        }
        EnvPreset::Sqlalchemy => {
            let credentials = match password {
                Some(password) => format!(
                    "{}:{}",
                    conn::url_component(user),
                    conn::url_component(password)
                ),
                None => conn::url_component(user),
            };
            push(
                "SQLALCHEMY_DATABASE_URI",
                format!(
                    "postgresql+psycopg://{credentials}@{}:{port}/{}{ssl}",
                    conn::url_host(host),
                    conn::url_component(database)
                ),
                password.is_some(),
            );
        }
        EnvPreset::DbParts => {
            push("DB_HOST", host.into(), false);
            push("DB_PORT", port.into(), false);
            push("DB_NAME", database.into(), false);
            push("DB_USER", user.into(), false);
            if let Some(password) = password {
                push("DB_PASSWORD", password.into(), true);
            }
        }
    }
    Ok(vars)
}

pub fn path(worktree: &Path) -> PathBuf {
    worktree.join(DIR).join(FILE)
}

/// Replace `.devflow/env` with `vars` (dotenv syntax, values single-quoted).
///
/// The generated file holds credentials, so nothing is written through a
/// symlink: `.devflow` must be a real directory and `.gitignore` / `env` real
/// files (or absent) inside the worktree, otherwise the write is refused.
pub fn write(worktree: &Path, vars: &[(String, String)]) -> Result<PathBuf> {
    let dir = worktree.join(DIR);
    match dir.symlink_metadata() {
        Ok(_) => ensure_plain_dir(&dir)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `create_dir`, not `create_dir_all`: never follow a path component
            // that appeared meanwhile.
            match std::fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure_plain_dir(&dir)?;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot create {}", dir.display()));
                }
            }
        }
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", dir.display())),
    }
    let ignore = dir.join(".gitignore");
    let path = dir.join(FILE);
    // Check both before writing either.
    let ignore_exists = ensure_plain_file(&ignore)?;
    ensure_plain_file(&path)?;
    if !ignore_exists {
        crate::fsutil::write_atomic(&ignore, b"*\n", false)?;
    }
    let mut text = String::from("# Generated by devflow; rewritten on setup. Do not edit.\n");
    for (key, value) in vars {
        text.push_str(key);
        text.push('=');
        text.push_str(&quote(value));
        text.push('\n');
    }
    // A temporary file in the verified directory, renamed over the target.
    crate::fsutil::write_atomic(&path, text.as_bytes(), true)?;
    Ok(path)
}

fn ensure_plain_dir(dir: &Path) -> Result<()> {
    let meta = dir
        .symlink_metadata()
        .with_context(|| format!("cannot inspect {}", dir.display()))?;
    if !meta.file_type().is_dir() {
        bail!(
            "{} is a symlink or not a directory; refusing to write credentials through it. \
             Remove it and re-run `devflow setup`",
            dir.display()
        );
    }
    Ok(())
}

/// Whether `path` exists; it must then be a regular file.
fn ensure_plain_file(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_file() => Ok(true),
        Ok(_) => bail!(
            "{} is a symlink or not a regular file; refusing to write through it. Remove it and re-run `devflow setup`",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

/// Read back `.devflow/env` (as written by [`write`]).
pub fn read(worktree: &Path) -> Result<Vec<(String, String)>> {
    let text = match std::fs::read_to_string(path(worktree)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), unquote(v)))
        .collect())
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .map(|v| v.replace("'\\''", "'"))
        .unwrap_or_else(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn section(presets: &[EnvPreset], vars: &[(&str, &str)]) -> EnvSection {
        EnvSection {
            presets: presets.to_vec(),
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    const URL: &str = "postgres://dfl_x:p%40ss%2Fw%3Ard@127.0.0.1:20001/df_proj_x"; // kingfisher:ignore
    const PASSWORD: &str = "p@ss/w:rd"; // kingfisher:ignore

    fn db_env() -> Vec<(String, String)> {
        let params = conn::Params::parse(URL).unwrap();
        let mut vars = generated(&[("DATABASE_URL", URL), ("DEVFLOW_WORKSPACE", "feature/a")]);
        vars.extend(params.env().into_iter().map(|(k, v)| (k.to_string(), v)));
        vars
    }

    fn value<'a>(rendered: &'a Rendered, key: &str) -> Option<&'a str> {
        rendered
            .vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn templates_split_into_text_and_exact_references() {
        use Segment::{Text, Var};
        let text = |s: &str| Text(s.to_string());
        let var = |s: &str| Var(s.to_string());
        assert_eq!(parse_template("").unwrap(), vec![]);
        assert_eq!(parse_template("plain").unwrap(), vec![text("plain")]);
        assert_eq!(
            parse_template("a${PGHOST}:${PGPORT}/x").unwrap(),
            vec![
                text("a"),
                var("PGHOST"),
                text(":"),
                var("PGPORT"),
                text("/x")
            ]
        );
        assert_eq!(
            parse_template("${DATABASE_URL}").unwrap(),
            vec![var("DATABASE_URL")]
        );
        // A lone `$` is text; `$${` is a literal `${`.
        assert_eq!(
            parse_template("cost $5 $").unwrap(),
            vec![text("cost $5 $")]
        );
        assert_eq!(
            parse_template("$${NOT_A_REF}").unwrap(),
            vec![text("${NOT_A_REF}")]
        );
        for bad in [
            "${A:-b}",
            "${A-b}",
            "${}",
            "${1A}",
            "${A B}",
            "${${A}}",
            "${A",
            "${A}${",
            "$(id)${a.b}",
        ] {
            assert!(parse_template(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn recipes_substitute_generated_variables_in_one_pass() {
        let env = db_env();
        let rendered = render(
            &section(
                &[],
                &[
                    ("DIRECT_URL", "${DATABASE_URL}"),
                    ("DB_HOST", "${PGHOST}"),
                    (
                        "API",
                        "http://${PGHOST}:${PGPORT}/${DEVFLOW_WORKSPACE}?x=$$1",
                    ),
                    ("LITERAL", "just text"),
                ],
            ),
            &env,
            Some(URL),
        );
        assert_eq!(value(&rendered, "DIRECT_URL"), Some(URL));
        assert_eq!(value(&rendered, "DB_HOST"), Some("127.0.0.1"));
        assert_eq!(
            value(&rendered, "API"),
            Some("http://127.0.0.1:20001/feature/a?x=$$1"),
            "a value is expanded once and `$$` stays as written"
        );
        assert_eq!(value(&rendered, "LITERAL"), Some("just text"));
        assert!(rendered.warnings.is_empty(), "{:?}", rendered.warnings);
        // Order is deterministic: recipes by name.
        let keys: Vec<_> = rendered.vars.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["API", "DB_HOST", "DIRECT_URL", "LITERAL"]);
    }

    #[test]
    fn values_built_from_credentials_are_secret_and_nothing_else_is() {
        let env = db_env();
        let rendered = render(
            &section(
                &[],
                &[
                    ("DIRECT_URL", "${DATABASE_URL}"),
                    ("AUTH", "${PGUSER}:${PGPASSWORD}"),
                    ("DB_HOST", "${PGHOST}"),
                    ("WHERE", "${DEVFLOW_WORKSPACE}"),
                ],
            ),
            &env,
            Some(URL),
        );
        let mut secret = rendered.secret_keys.clone();
        secret.sort();
        assert_eq!(secret, ["AUTH", "DIRECT_URL"]);
        assert!(rendered.secrets.contains(&URL.to_string()));
        assert!(rendered.secrets.contains(&format!("dfl_x:{PASSWORD}")));
        assert!(!rendered.secrets.iter().any(|s| s == "127.0.0.1"));
    }

    #[test]
    fn a_recipe_whose_reference_has_no_value_here_is_left_out_with_a_warning() {
        // A `--no-db` workspace: no DATABASE_URL, no PG*.
        let env = generated(&[("DEVFLOW_WORKSPACE", "w")]);
        let rendered = render(
            &section(
                &[],
                &[
                    ("DIRECT_URL", "${DATABASE_URL}"),
                    ("WHERE", "${DEVFLOW_WORKSPACE}"),
                ],
            ),
            &env,
            None,
        );
        assert_eq!(value(&rendered, "DIRECT_URL"), None);
        assert_eq!(value(&rendered, "WHERE"), Some("w"));
        assert_eq!(rendered.warnings.len(), 1);
        assert!(
            rendered.warnings[0].contains("env.DIRECT_URL")
                && rendered.warnings[0].contains("DATABASE_URL"),
            "{:?}",
            rendered.warnings
        );
    }

    #[test]
    fn presets_render_typed_fields_from_the_connection() {
        let env = db_env();
        let all = EnvPreset::ALL;
        // `laravel` and `db_parts` share keys; render them apart.
        let laravel = render(&section(&[EnvPreset::Laravel], &[]), &env, Some(URL));
        let get = |r: &Rendered, k: &str| value(r, k).map(str::to_string);
        assert_eq!(get(&laravel, "DB_CONNECTION").as_deref(), Some("pgsql"));
        assert_eq!(get(&laravel, "DB_HOST").as_deref(), Some("127.0.0.1"));
        assert_eq!(get(&laravel, "DB_PORT").as_deref(), Some("20001"));
        assert_eq!(get(&laravel, "DB_DATABASE").as_deref(), Some("df_proj_x"));
        assert_eq!(get(&laravel, "DB_USERNAME").as_deref(), Some("dfl_x"));
        assert_eq!(get(&laravel, "DB_PASSWORD").as_deref(), Some(PASSWORD));
        assert_eq!(laravel.secret_keys, ["DB_PASSWORD"]);

        let db_parts = render(&section(&[EnvPreset::DbParts], &[]), &env, Some(URL));
        assert_eq!(get(&db_parts, "DB_NAME").as_deref(), Some("df_proj_x"));
        assert_eq!(get(&db_parts, "DB_USER").as_deref(), Some("dfl_x"));
        assert_eq!(get(&db_parts, "DB_PASSWORD").as_deref(), Some(PASSWORD));

        let prisma = render(&section(&[EnvPreset::Prisma], &[]), &env, Some(URL));
        assert_eq!(
            get(&prisma, "DIRECT_URL").as_deref(),
            Some(URL),
            "the same URL"
        );
        assert_eq!(
            get(&prisma, "SHADOW_DATABASE_URL"),
            None,
            "no shadow database configured: nothing to export"
        );
        assert_eq!(prisma.secret_keys, ["DIRECT_URL"]);

        let spring = render(&section(&[EnvPreset::Spring], &[]), &env, Some(URL));
        assert_eq!(
            get(&spring, "SPRING_DATASOURCE_URL").as_deref(),
            Some("jdbc:postgresql://127.0.0.1:20001/df_proj_x")
        );
        assert_eq!(
            get(&spring, "SPRING_DATASOURCE_USERNAME").as_deref(),
            Some("dfl_x")
        );
        assert_eq!(
            get(&spring, "SPRING_DATASOURCE_PASSWORD").as_deref(),
            Some(PASSWORD)
        );
        assert_eq!(spring.secret_keys, ["SPRING_DATASOURCE_PASSWORD"]);

        // The URI is percent-encoded component by component.
        let sqlalchemy = render(&section(&[EnvPreset::Sqlalchemy], &[]), &env, Some(URL));
        assert_eq!(
            get(&sqlalchemy, "SQLALCHEMY_DATABASE_URI").as_deref(),
            Some("postgresql+psycopg://dfl_x:p%40ss%2Fw%3Ard@127.0.0.1:20001/df_proj_x")
        );
        assert_eq!(sqlalchemy.secret_keys, ["SQLALCHEMY_DATABASE_URI"]);
        assert_eq!(all.len(), 5);
    }

    const SHADOW_URL: &str = "postgres://dfl_x:p%40ss%2Fw%3Ard@127.0.0.1:20001/df_proj_x_2"; // kingfisher:ignore

    #[test]
    fn scratch_urls_are_generated_variables_that_recipes_and_presets_work_with() {
        // devflow generates `<NAME>_DATABASE_URL` for each scratch database,
        // from the binding's login: the password is the binding's, so the
        // value is secret like DATABASE_URL.
        let mut env = db_env();
        env.push(("SHADOW_DATABASE_URL".into(), SHADOW_URL.into()));
        let rendered = render(
            &section(
                &[EnvPreset::Prisma],
                &[
                    ("SHADOW_COPY", "${SHADOW_DATABASE_URL}"),
                    ("PLAIN", "${DEVFLOW_WORKSPACE}"),
                ],
            ),
            &env,
            Some(URL),
        );
        // The preset adds DIRECT_URL and leaves the exported shadow URL as it
        // is: no duplicate, no warning.
        assert_eq!(value(&rendered, "DIRECT_URL"), Some(URL));
        assert_eq!(value(&rendered, "SHADOW_DATABASE_URL"), None);
        assert!(rendered.warnings.is_empty(), "{:?}", rendered.warnings);
        // A recipe can pass it on, and the copy is secret.
        assert_eq!(value(&rendered, "SHADOW_COPY"), Some(SHADOW_URL));
        assert!(rendered.secret_keys.contains(&"SHADOW_COPY".to_string()));
        assert!(rendered.secrets.contains(&SHADOW_URL.to_string()));
        assert!(!rendered.secret_keys.contains(&"PLAIN".to_string()));
        // Without the scratch database the reference has no value.
        let without = render(
            &section(&[], &[("S", "${SHADOW_DATABASE_URL}")]),
            &db_env(),
            None,
        );
        assert_eq!(value(&without, "S"), None);
        assert!(without.warnings[0].contains("SHADOW_DATABASE_URL"));
    }

    #[test]
    fn presets_encode_awkward_names_and_carry_sslmode_and_ipv6_hosts() {
        let url = "postgres://us%40er:pw%23%3F@[::1]:6543/my%20db?sslmode=require"; // kingfisher:ignore
        let env = generated(&[("DATABASE_URL", url)]);
        let rendered = render(
            &section(&[EnvPreset::Sqlalchemy, EnvPreset::Spring], &[]),
            &env,
            Some(url),
        );
        assert_eq!(
            value(&rendered, "SQLALCHEMY_DATABASE_URI"),
            Some("postgresql+psycopg://us%40er:pw%23%3F@[::1]:6543/my%20db?sslmode=require")
        );
        assert_eq!(
            value(&rendered, "SPRING_DATASOURCE_URL"),
            Some("jdbc:postgresql://[::1]:6543/my%20db?sslmode=require")
        );
        assert_eq!(
            value(&rendered, "SPRING_DATASOURCE_USERNAME"),
            Some("us@er")
        );
        // Prisma keeps its own options (`schema=…`), as written.
        let prisma_url = "postgres://u:pw@h:1/d?schema=public"; // kingfisher:ignore
        let prisma = render(
            &section(&[EnvPreset::Prisma], &[]),
            &generated(&[("DATABASE_URL", prisma_url)]),
            Some(prisma_url),
        );
        assert_eq!(value(&prisma, "DIRECT_URL"), Some(prisma_url));
        // A URL without a password marks nothing secret and has no password field.
        let open = "postgres://u@h:1/d";
        let rendered = render(
            &section(&[EnvPreset::Laravel, EnvPreset::Prisma], &[]),
            &generated(&[("DATABASE_URL", open)]),
            Some(open),
        );
        assert_eq!(value(&rendered, "DB_PASSWORD"), None);
        assert_eq!(value(&rendered, "DB_PORT"), Some("1"));
        assert!(
            rendered.secret_keys.is_empty(),
            "{:?}",
            rendered.secret_keys
        );
        // The port defaults like libpq's.
        let no_port = "postgres://u@h/d";
        let rendered = render(
            &section(&[EnvPreset::DbParts], &[]),
            &generated(&[("DATABASE_URL", no_port)]),
            Some(no_port),
        );
        assert_eq!(value(&rendered, "DB_PORT"), Some("5432"));
    }

    #[test]
    fn presets_without_a_postgres_binding_are_skipped_with_a_warning() {
        let env = generated(&[("DEVFLOW_WORKSPACE", "w")]);
        let rendered = render(
            &section(&[EnvPreset::Laravel, EnvPreset::Prisma], &[("OTHER", "x")]),
            &env,
            None,
        );
        assert_eq!(rendered.vars, [("OTHER".to_string(), "x".to_string())]);
        assert_eq!(rendered.warnings.len(), 1);
        assert!(
            rendered.warnings[0].contains("laravel, prisma")
                && rendered.warnings[0].contains("no postgres binding"),
            "{:?}",
            rendered.warnings
        );
        // An unreadable or incomplete URL names what is missing.
        let rendered = render(
            &section(&[EnvPreset::DbParts], &[]),
            &env,
            Some("postgres://h/d"),
        );
        assert!(rendered.vars.is_empty());
        assert!(
            rendered.warnings[0].contains("no user"),
            "{:?}",
            rendered.warnings
        );
    }

    #[test]
    fn generated_and_reserved_names() {
        for name in [
            "DATABASE_URL",
            "PGHOST",
            "PGPASSWORD",
            "PGSSLMODE",
            "DEVFLOW_WORKSPACE",
            "COMPOSE_PROJECT_NAME",
        ] {
            assert!(is_generated(name) && is_reserved(name), "{name}");
        }
        // Reserved without being generated here: other devflow and libpq names.
        for name in [
            "PGSERVICE",
            "DEVFLOW_ANYTHING",
            "TEST_DATABASE_URL",
            "SHADOW_DATABASE_URL",
        ] {
            assert!(is_reserved(name) && !is_generated(name), "{name}");
        }
        for name in ["DIRECT_URL", "DB_HOST", "NODE_OPTIONS", "pg_lower"] {
            assert!(!is_reserved(name), "{name}");
        }
    }

    #[test]
    fn round_trips_quoted_values_and_ignores_itself() {
        let dir = tempfile::tempdir().unwrap();
        let vars = vec![
            (
                "DATABASE_URL".to_string(),
                "postgres://u:p@h:1/d".to_string(),
            ),
            ("QUOTED".to_string(), "it's".to_string()),
        ];
        write(dir.path(), &vars).unwrap();
        assert_eq!(read(dir.path()).unwrap(), vars);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".devflow/.gitignore")).unwrap(),
            "*\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn never_writes_through_symlinks_or_into_non_directories() {
        use std::os::unix::fs::symlink;
        let vars = vec![("A".to_string(), "secret".to_string())];
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("env");
        std::fs::write(&sentinel, "untouched").unwrap();

        // `.devflow` is a symlink to a directory elsewhere.
        let work = tempfile::tempdir().unwrap();
        symlink(outside.path(), work.path().join(DIR)).unwrap();
        let error = write(work.path(), &vars).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "untouched");
        assert!(!outside.path().join(".gitignore").exists());

        // ...to a missing target, or to a file.
        std::fs::remove_file(work.path().join(DIR)).unwrap();
        symlink(outside.path().join("missing"), work.path().join(DIR)).unwrap();
        assert!(write(work.path(), &vars).is_err());
        assert!(!outside.path().join("missing").exists());
        std::fs::remove_file(work.path().join(DIR)).unwrap();
        symlink(&sentinel, work.path().join(DIR)).unwrap();
        assert!(write(work.path(), &vars).is_err());
        std::fs::remove_file(work.path().join(DIR)).unwrap();

        // `.devflow` is a regular file.
        std::fs::write(work.path().join(DIR), "x").unwrap();
        assert!(write(work.path(), &vars).is_err());
        std::fs::remove_file(work.path().join(DIR)).unwrap();

        // `.devflow/env` and `.devflow/.gitignore` are symlinks.
        std::fs::create_dir(work.path().join(DIR)).unwrap();
        symlink(&sentinel, work.path().join(DIR).join(FILE)).unwrap();
        let error = write(work.path(), &vars).unwrap_err();
        assert!(error.to_string().contains("regular file"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "untouched");
        std::fs::remove_file(work.path().join(DIR).join(FILE)).unwrap();
        symlink(&sentinel, work.path().join(DIR).join(".gitignore")).unwrap();
        assert!(write(work.path(), &vars).is_err());
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "untouched");
        std::fs::remove_file(work.path().join(DIR).join(".gitignore")).unwrap();

        // A plain directory works, and rewriting it keeps working.
        write(work.path(), &vars).unwrap();
        write(work.path(), &vars).unwrap();
        assert_eq!(read(work.path()).unwrap(), vars);
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "untouched");
    }
}
