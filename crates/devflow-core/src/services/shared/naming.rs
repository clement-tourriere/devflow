//! Pure, unit-testable naming and SQL helpers for the shared-postgres
//! provider. No Docker or I/O here so the logic can be tested in isolation.

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
    let project = sanitize_fragment(project);
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

    truncate_ident(&name)
}

/// The prefix shared by every logical database of a project: `<project>_`.
/// Used to list/GC a project's databases.
pub fn project_db_prefix(project: &str) -> String {
    format!("{}_", sanitize_fragment(project))
}

/// Truncate to the postgres identifier byte limit on a char boundary.
fn truncate_ident(name: &str) -> String {
    if name.len() <= PG_IDENT_MAX {
        return name.to_string();
    }
    let mut end = PG_IDENT_MAX;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].trim_end_matches('_').to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

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
