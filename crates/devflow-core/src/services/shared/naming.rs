//! Pure, unit-testable naming and SQL helpers for the shared-postgres
//! provider. No Docker or I/O here so the logic can be tested in isolation.

use sha2::{Digest, Sha256};

/// Max length of a PostgreSQL identifier (bytes). Names are truncated to fit.
const PG_IDENT_MAX: usize = 63;

/// Sanitize a single name component into a postgres-identifier-safe fragment:
/// lowercase ASCII alphanumerics and underscores only, with runs of other
/// characters collapsed to a single `_`.
fn sanitize_fragment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_underscore = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Build the logical database name for a (project, workspace) pair:
/// `<project>_<workspace>`, postgres-identifier-safe and ≤63 bytes.
///
/// PostgreSQL identifiers must not start with a digit (unquoted) — we always
/// quote them at the call site, but a leading digit is still avoided so the
/// name is valid everywhere. Empty results fall back to a stable default.
pub fn logical_db_name(project: &str, workspace: &str) -> String {
    let project = bounded_project_fragment(&sanitize_fragment(project), '_');
    let workspace = sanitize_fragment(workspace);

    let mut name = match (project.is_empty(), workspace.is_empty()) {
        (true, true) => "devflow_db".to_string(),
        (true, false) => workspace,
        (false, true) => project,
        (false, false) => format!("{project}_{workspace}"),
    };

    // Avoid a leading digit.
    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        name.insert_str(0, "db_");
    }

    stable_truncate(&name, PG_IDENT_MAX, '_')
}

/// The prefix shared by every logical database of a project: `<project>_`.
/// Used to list/GC a project's databases.
pub fn project_db_prefix(project: &str) -> String {
    format!(
        "{}_",
        bounded_project_fragment(&sanitize_fragment(project), '_')
    )
}

/// Keep long project names useful as list/GC prefixes while reserving room
/// for a workspace fragment. The digest prevents two equally-prefixed project
/// names from sharing a global-engine namespace.
///
/// MIGRATION CONSTRAINT: releases before hash-bounded naming used the full
/// sanitized project fragment, and existing databases/buckets carry those
/// names. Hashing must therefore only kick in where the old scheme's list/GC
/// prefix contract was already broken. `stable_truncate` preserves the first
/// `max - 1 - 12` bytes (50 for the 63-byte limits used here), so any project
/// fragment ≤ 49 bytes keeps prefix-matching every truncated name — bound at
/// exactly that, NOT lower, or projects between the bound and 49 bytes get
/// silently renamed and their existing data orphaned.
fn bounded_project_fragment(project: &str, separator: char) -> String {
    const PROJECT_MAX: usize = 49;
    if project.len() <= PROJECT_MAX {
        return project.to_string();
    }
    let digest = short_digest(project);
    let prefix_len = PROJECT_MAX - 1 - digest.len();
    format!("{}{separator}{digest}", &project[..prefix_len])
}

/// Truncate an ASCII-safe identifier without throwing away its distinguishing
/// suffix. Hashing the full value keeps long project/workspace pairs unique
/// even when their readable prefixes are identical. (Names over `max` were
/// plainly cut by pre-hash releases — and could collide; only that population
/// changes names across the upgrade.)
pub(crate) fn stable_truncate(name: &str, max: usize, separator: char) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    let digest = short_digest(name);
    let prefix_len = max - 1 - digest.len();
    format!(
        "{}{separator}{digest}",
        name[..prefix_len].trim_end_matches(separator)
    )
}

fn short_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Quote a postgres identifier by doubling embedded double-quotes and wrapping
/// in `"`. Our names are already sanitized, but quoting is defense-in-depth.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a string literal for SQL by doubling embedded single-quotes.
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// SQL to create a logical database, optionally cloned from a parent via
/// `TEMPLATE` (postgres' built-in branch-from-parent).
pub fn create_database_sql(db: &str, template: Option<&str>) -> String {
    match template {
        Some(parent) => format!(
            "CREATE DATABASE {} TEMPLATE {}",
            quote_ident(db),
            quote_ident(parent)
        ),
        None => format!("CREATE DATABASE {}", quote_ident(db)),
    }
}

/// SQL to forcibly terminate all backends connected to a database (so it can
/// be dropped or used as a TEMPLATE source).
pub fn terminate_connections_sql(db: &str) -> String {
    format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = {} AND pid <> pg_backend_pid()",
        quote_literal(db)
    )
}

/// SQL that returns one row iff the database exists.
pub fn database_exists_sql(db: &str) -> String {
    format!(
        "SELECT 1 FROM pg_database WHERE datname = {}",
        quote_literal(db)
    )
}

/// SQL to list databases belonging to a project (by name prefix).
pub fn list_databases_sql(prefix: &str) -> String {
    // LIKE with the prefix; `_` is a wildcard in LIKE so escape it.
    let escaped = prefix.replace('_', "\\_");
    format!(
        "SELECT datname FROM pg_database WHERE datname LIKE {} ORDER BY datname",
        quote_literal(&format!("{escaped}%"))
    )
}

// ── S3 / object-storage bucket naming ───────────────────────────────────

/// Min/max length of an S3 bucket name.
const S3_BUCKET_MIN: usize = 3;
const S3_BUCKET_MAX: usize = 63;

/// Build the per-workspace bucket name for a (project, workspace) pair.
///
/// S3 bucket names are stricter than postgres identifiers: 3–63 chars,
/// lowercase letters/digits/hyphens only, must start and end with a letter or
/// digit, and no underscores or consecutive dots. We use `-` as the separator
/// (not `_` like databases).
pub fn logical_bucket_name(project: &str, workspace: &str) -> String {
    let project = bounded_project_fragment(&sanitize_bucket_fragment(project), '-');
    let workspace = sanitize_bucket_fragment(workspace);

    let mut name = match (project.is_empty(), workspace.is_empty()) {
        (true, true) => "devflow-bucket".to_string(),
        (true, false) => workspace,
        (false, true) => project,
        (false, false) => format!("{project}-{workspace}"),
    };

    name = stable_truncate(&name, S3_BUCKET_MAX, '-');
    name = name.trim_matches('-').to_string();
    // Pad if the trimmed name fell below the minimum length.
    while name.len() < S3_BUCKET_MIN {
        name.push('0');
    }
    name
}

/// The shared prefix of every bucket for a project: `<project>-`.
pub fn project_bucket_prefix(project: &str) -> String {
    format!(
        "{}-",
        bounded_project_fragment(&sanitize_bucket_fragment(project), '-')
    )
}

// ── Redis allocation identity ───────────────────────────────────────────

/// The allocation-hash field identifying a (project, workspace) pair for the
/// shared Redis DB-index allocator: `<project>:<workspace>`. Both fragments are
/// sanitized to contain no `:` so the field parses unambiguously on the colon.
pub fn redis_alloc_field(project: &str, workspace: &str) -> String {
    format!(
        "{}:{}",
        sanitize_bucket_fragment(project),
        sanitize_bucket_fragment(workspace)
    )
}

/// The field prefix shared by all of a project's Redis allocations: `<project>:`.
pub fn redis_project_prefix(project: &str) -> String {
    format!("{}:", sanitize_bucket_fragment(project))
}

/// Sanitize a fragment for an S3 bucket name: lowercase alphanumerics and
/// hyphens, collapsing runs of other characters to a single `-`.
fn sanitize_bucket_fragment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logical_bucket_name_basic() {
        assert_eq!(logical_bucket_name("myapp", "main"), "myapp-main");
        // Workspace slashes/underscores become hyphens (S3 forbids underscores).
        assert_eq!(
            logical_bucket_name("My_App", "feature/auth"),
            "my-app-feature-auth"
        );
    }

    #[test]
    fn test_logical_bucket_name_constraints() {
        // 3..=63 chars, lowercase, no leading/trailing hyphen.
        let n = logical_bucket_name("x", "");
        assert!(n.len() >= 3, "min length: {n}");
        let long = logical_bucket_name(&"a".repeat(80), &"b".repeat(80));
        assert!(long.len() <= 63);
        assert!(!long.starts_with('-') && !long.ends_with('-'));
        assert!(long
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        let a = logical_bucket_name(
            &"project".repeat(20),
            &format!("{}-a", "feature".repeat(20)),
        );
        let b = logical_bucket_name(
            &"project".repeat(20),
            &format!("{}-b", "feature".repeat(20)),
        );
        assert_ne!(a, b, "long bucket identities must retain uniqueness");
    }

    #[test]
    fn test_project_bucket_prefix() {
        assert_eq!(project_bucket_prefix("My_App"), "my-app-");
    }

    #[test]
    fn test_redis_alloc_field() {
        assert_eq!(
            redis_alloc_field("My_App", "feature/auth"),
            "my-app:feature-auth"
        );
        assert_eq!(redis_project_prefix("My_App"), "my-app:");
        // The field has exactly one colon (fragments contain none), so it
        // round-trips by splitting on ':'.
        let f = redis_alloc_field("proj", "ws");
        assert_eq!(f.matches(':').count(), 1);
    }

    #[test]
    fn test_logical_db_name_basic() {
        assert_eq!(logical_db_name("myapp", "main"), "myapp_main");
        assert_eq!(
            logical_db_name("myapp", "feature/auth"),
            "myapp_feature_auth"
        );
    }

    #[test]
    fn test_logical_db_name_sanitizes_and_collapses() {
        assert_eq!(
            logical_db_name("My-App", "Feature/Auth-2"),
            "my_app_feature_auth_2"
        );
        // Runs of separators collapse to a single underscore.
        assert_eq!(logical_db_name("a//b", "c..d"), "a_b_c_d");
    }

    #[test]
    fn test_logical_db_name_avoids_leading_digit() {
        assert_eq!(logical_db_name("123proj", "ws"), "db_123proj_ws");
    }

    #[test]
    fn test_logical_db_name_truncates_to_63() {
        let long = "x".repeat(100);
        let name = logical_db_name(&long, "ws");
        assert!(name.len() <= 63, "got {} bytes", name.len());
        assert_ne!(
            logical_db_name(&"project".repeat(20), &format!("{}a", "feature".repeat(20))),
            logical_db_name(&"project".repeat(20), &format!("{}b", "feature".repeat(20)))
        );
        assert!(name.starts_with(project_db_prefix(&long).trim_end_matches('_')));
    }

    #[test]
    fn test_logical_db_name_empty_fallbacks() {
        assert_eq!(logical_db_name("", ""), "devflow_db");
        assert_eq!(logical_db_name("", "main"), "main");
        assert_eq!(logical_db_name("proj", ""), "proj");
    }

    #[test]
    fn test_project_db_prefix() {
        assert_eq!(project_db_prefix("My-App"), "my_app_");
    }

    #[test]
    fn test_mid_length_project_names_unchanged_from_plain_scheme() {
        // Projects with ≤49 sanitized bytes must keep their historical
        // (pre-hash) names whenever the total fits the identifier limit:
        // renaming them would orphan databases/buckets created by earlier
        // releases and break their list/GC prefixes.
        let project = "my-organization-analytics-platform";
        assert_eq!(
            logical_db_name(project, "main"),
            "my_organization_analytics_platform_main"
        );
        assert_eq!(
            project_db_prefix(project),
            "my_organization_analytics_platform_"
        );
        assert_eq!(
            logical_bucket_name(project, "main"),
            "my-organization-analytics-platform-main"
        );
        assert_eq!(
            project_bucket_prefix(project),
            "my-organization-analytics-platform-"
        );
    }

    #[test]
    fn test_create_database_sql() {
        assert_eq!(
            create_database_sql("myapp_feat", None),
            "CREATE DATABASE \"myapp_feat\""
        );
        assert_eq!(
            create_database_sql("myapp_feat", Some("myapp_main")),
            "CREATE DATABASE \"myapp_feat\" TEMPLATE \"myapp_main\""
        );
    }

    #[test]
    fn test_quote_literal_escapes_quotes() {
        assert_eq!(quote_literal("a'b"), "'a''b'");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn test_list_databases_sql_escapes_underscore() {
        // The project prefix underscore must be escaped so LIKE treats it
        // literally, not as a single-char wildcard.
        let sql = list_databases_sql("my_app_");
        assert!(sql.contains("my\\_app\\_%"), "got: {sql}");
    }

    #[test]
    fn test_database_exists_sql() {
        assert_eq!(
            database_exists_sql("myapp_main"),
            "SELECT 1 FROM pg_database WHERE datname = 'myapp_main'"
        );
    }
}
