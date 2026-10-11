//! The MySQL 8.4 storage profile: standalone InnoDB, one server per branch.
//!
//! devflow generates every container's configuration, so it controls
//! persistence: a `my.cnf` mounted read-only (no binary log, no replication,
//! a small memory footprint, a slow InnoDB shutdown), a root and an
//! application account whose passwords are the branch's own, and nothing else
//! in `mysql.user`. A server that is not shaped like this is refused rather
//! than copied wrongly.
//!
//! Consistency: the source is shut down cleanly (`docker stop`, verified by
//! its exit code and InnoDB's own shutdown message), its whole data directory
//! is reflinked inside the Docker VM, and the source is started again. The
//! interruption is measured and reported. `docker pause` is not a storage
//! barrier for InnoDB, so there is no pause profile.
//!
//! Identity: a data directory carries the server's UUID (`auto.cnf`), its TLS
//! and RSA key pairs, its accounts and their passwords. A clone drops the
//! files before it ever starts, and a bootstrap server with no network and no
//! grant tables sets new passwords through its local socket before the clone
//! listens anywhere.
use super::Service;
use super::catalog::{BranchRecord, Op, ResetPlan, SourceRef};
use super::docker::{self, ContainerInfo, ContainerSpec};
use super::exec::Stopped;
use super::launch::VolumeOwner;
use crate::config::Engine;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The port the server listens on inside the container.
pub const CONTAINER_PORT: u16 = 3306;
/// Where the data volume is mounted in the container.
pub const DATA_DIR: &str = "/var/lib/mysql";
/// The application account every branch has, reachable from any host (the
/// container is published on loopback only).
pub const APP_USER: &str = "app";
/// The file the profile is written to, in devflow's data directory, and where
/// it is mounted (read-only) in every MySQL container.
pub const PROFILE_FILE: &str = "mysql-profile-v1.cnf";
pub const PROFILE_MOUNT: &str = "/etc/mysql/conf.d/devflow.cnf";
/// The profile's name, for reports.
pub const PROFILE: &str = "mysql-8.4-standalone-innodb";

/// The generated `my.cnf`. Everything here is a decision: a branch is a
/// development database, so the buffer pool is small and the server is idle
/// most of the time; durability stays at its defaults (`innodb_flush_log_at_trx_commit=1`,
/// doublewrite on), because the clone's consistency rests on a clean
/// shutdown, not on settings that weaken it.
pub const CNF: &str = "\
# devflow-managed: profile mysql-8.4-standalone-innodb v1. Generated, mounted
# read-only into every MySQL branch container; changes are overwritten.
[mysqld]
user=mysql
datadir=/var/lib/mysql
socket=/var/run/mysqld/mysqld.sock
# Inside the container: it is published on 127.0.0.1 of the host only.
bind-address=0.0.0.0
skip-name-resolve
# Standalone: no binary log, no X plugin.
skip-log-bin
mysqlx=OFF
# InnoDB's own messages (the shutdown report a clone relies on) are notes.
log_error_verbosity=3
# InnoDB only. A clone shuts the server down, and a MEMORY table loses its rows
# when it does; the other engines are not part of the cloning contract either.
# A table cannot be created with, or altered to, one of them.
default_storage_engine=InnoDB
disabled_storage_engines=\"MyISAM,MEMORY,BLACKHOLE,ARCHIVE,CSV,FEDERATED,MERGE\"
# A small footprint for an idle development server.
performance_schema=OFF
innodb_buffer_pool_size=64M
innodb_buffer_pool_instances=1
innodb_buffer_pool_dump_at_shutdown=OFF
innodb_buffer_pool_load_at_startup=OFF
innodb_log_buffer_size=4M
innodb_read_io_threads=2
innodb_write_io_threads=2
innodb_purge_threads=1
innodb_page_cleaners=1
key_buffer_size=1M
max_connections=50
table_open_cache=256
table_definition_cache=400
tmp_table_size=16M
max_heap_table_size=16M
thread_cache_size=4
# A slow shutdown: everything is purged and merged, so the data directory a
# clone copies needs no recovery at all.
innodb_fast_shutdown=0

[client]
socket=/var/run/mysqld/mysqld.sock
";

/// Accounts MySQL creates itself. A branch has these, `root@localhost` and
/// the application account, and nothing else.
const SYSTEM_ACCOUNTS: [(&str, &str); 3] = [
    ("mysql.infoschema", "localhost"),
    ("mysql.session", "localhost"),
    ("mysql.sys", "localhost"),
];

/// The pieces of a MySQL connection URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub user: String,
    pub password: String,
    pub host: String,
    pub port: u16,
    pub database: String,
}

/// `mysql://<user>:<password>@<host>:<port>/<database>`, with each piece
/// percent-encoded.
pub fn url(user: &str, password: &str, host: &str, port: u16, database: &str) -> String {
    let enc = |piece: &str| utf8_percent_encode(piece, NON_ALPHANUMERIC).to_string();
    format!(
        "mysql://{}:{}@{host}:{port}/{}",
        enc(user),
        enc(password),
        enc(database)
    )
}

/// The inverse of [`url`]. `None` for anything else.
pub fn parse_url(text: &str) -> Option<UrlParts> {
    let rest = text.strip_prefix("mysql://")?;
    let (userinfo, rest) = rest.split_once('@')?;
    let (user, password) = userinfo.split_once(':')?;
    let (hostport, database) = rest.split_once('/')?;
    let (host, port) = hostport.rsplit_once(':')?;
    let dec = |piece: &str| {
        percent_decode_str(piece)
            .decode_utf8()
            .ok()
            .map(String::from)
    };
    Some(UrlParts {
        user: dec(user)?,
        password: dec(password)?,
        host: host.to_string(),
        port: port.parse().ok()?,
        database: dec(database.split('?').next().unwrap_or(database))?,
    })
}

/// The typed connection fields of a branch, as `.devflow/env` exports them
/// (`MYSQL_HOST`, `MYSQL_PORT`, `MYSQL_USER`, `MYSQL_PASSWORD`,
/// `MYSQL_DATABASE`), read from its URL.
pub fn env_fields(url: &str) -> Option<Vec<(&'static str, String)>> {
    let parts = parse_url(url)?;
    Some(vec![
        ("MYSQL_HOST", parts.host),
        ("MYSQL_PORT", parts.port.to_string()),
        ("MYSQL_USER", parts.user),
        ("MYSQL_PASSWORD", parts.password),
        ("MYSQL_DATABASE", parts.database),
    ])
}

/// A string literal for SQL: single quotes, with `\` and `'` escaped.
pub fn quote_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

/// An identifier for SQL: backticks, with `` ` `` doubled.
pub fn quote_ident(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

/// The accounts in `rows` (`user<TAB>host` per line, as the client prints
/// them) that a branch must not have: everything but MySQL's own accounts,
/// `root@localhost` and the application account. Returns `'user'@'host'`.
pub fn extra_accounts(rows: &str, app_user: &str) -> Vec<String> {
    let mut extras: Vec<String> = rows
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let (user, host) = line.split_once('\t').unwrap_or((line, ""));
            let known = SYSTEM_ACCOUNTS.contains(&(user, host))
                || (user, host) == ("root", "localhost")
                || (user, host) == (app_user, "%");
            (!known).then(|| format!("'{user}'@'{host}'"))
        })
        .collect();
    extras.sort();
    extras.dedup();
    extras
}

/// What a branch's accounts and schema are, to be set before the server ever
/// listens on a network.
#[derive(Clone)]
pub struct Identity<'a> {
    pub database: &'a str,
    pub user: &'a str,
    pub app_password: &'a str,
    pub root_password: &'a str,
}

/// The statements that give a server its branch identity, run through the
/// local socket of a bootstrap server that has no grant tables. `FLUSH
/// PRIVILEGES` loads them first, so the account statements are allowed;
/// every statement is idempotent. The passwords are in this text and nowhere
/// else: it travels on the standard input of `docker exec`, never on a
/// command line or in an environment.
pub fn identity_sql(identity: &Identity<'_>) -> String {
    let database = quote_ident(identity.database);
    let app = format!("{}@'%'", quote_str(identity.user));
    let root = "'root'@'localhost'";
    let mut sql = format!(
        "FLUSH PRIVILEGES;\n\
         ALTER USER {root} IDENTIFIED WITH caching_sha2_password BY {root_pw} {AUTH_OPTIONS};\n\
         ALTER USER {root} DISCARD OLD PASSWORD;\n\
         CREATE DATABASE IF NOT EXISTS {database};\n\
         CREATE USER IF NOT EXISTS {app} IDENTIFIED WITH caching_sha2_password BY {pw};\n\
         ALTER USER {app} IDENTIFIED WITH caching_sha2_password BY {pw} {AUTH_OPTIONS};\n\
         ALTER USER {app} DISCARD OLD PASSWORD;\n\
         GRANT ALL PRIVILEGES ON {database}.* TO {app};\n",
        root_pw = quote_str(identity.root_password),
        pw = quote_str(identity.app_password),
    );
    // MySQL's own accounts cannot be logged into whatever their source made
    // of them: locked, and with no secondary password.
    for (user, host) in SYSTEM_ACCOUNTS {
        sql.push_str(&format!(
            "ALTER USER '{user}'@'{host}' DISCARD OLD PASSWORD;\n\
             ALTER USER '{user}'@'{host}' ACCOUNT LOCK;\n"
        ));
    }
    sql.push_str("FLUSH PRIVILEGES;\n");
    sql
}

/// What every managed account's authentication is normalized to, whatever the
/// source had: the plugin is named beside the password, no required TLS
/// certificate, no password expiry or lock-out, the account unlocked.
/// `IDENTIFIED BY` alone would leave the source's other settings, and a
/// secondary password (`RETAIN CURRENT PASSWORD`) it kept, in the clone; the
/// secondary is discarded by its own statement.
const AUTH_OPTIONS: &str = "REQUIRE NONE PASSWORD EXPIRE NEVER PASSWORD REQUIRE CURRENT DEFAULT \
                            FAILED_LOGIN_ATTEMPTS 0 PASSWORD_LOCK_TIME 0 ACCOUNT UNLOCK";

/// The statements that read what a data directory grants beyond accounts:
/// roles (granted and default) and proxy privileges. One line per row,
/// `kind<TAB>value`; separate statements, because tables of different
/// collations cannot be joined in one `UNION`.
pub const GRAPH_AUDIT_SQL: &str = "\
SELECT 'role_edge', CONCAT(FROM_USER, '@', FROM_HOST, ' -> ', TO_USER, '@', TO_HOST) FROM mysql.role_edges;\n\
SELECT 'default_role', CONCAT(USER, '@', HOST, ' -> ', DEFAULT_ROLE_USER, '@', DEFAULT_ROLE_HOST) FROM mysql.default_roles;\n\
SELECT 'proxy', CONCAT_WS(',', Host, User, Proxied_host, Proxied_user, With_grant) FROM mysql.proxies_priv;\n";

/// The one proxy row a fresh MySQL 8.4 has: `root@localhost` may proxy
/// `''@''` (host, user, proxied host, proxied user, with grant).
const DEFAULT_PROXY: &str = "localhost,root,,,1";

/// Fail with the roles and proxy privileges that make a data directory unfit
/// to be a branch. `rows` is the output of [`GRAPH_AUDIT_SQL`].
pub fn require_no_roles_or_proxies(rows: &str) -> Result<()> {
    let mut found: Vec<String> = rows
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let (kind, value) = line.split_once('\t').unwrap_or((line, ""));
            match kind {
                "role_edge" => Some(format!("role grant {value}")),
                "default_role" => Some(format!("default role {value}")),
                "proxy" if value == DEFAULT_PROXY => None,
                "proxy" => Some(format!("proxy privilege ({value})")),
                other => Some(format!("{other} {value}")),
            }
        })
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    found.sort();
    bail!(
        "the data being cloned has role grants or proxy privileges devflow did not create: {}. \
         A branch has no roles and no proxy privilege beyond MySQL's default for root@localhost, \
         because they would carry their source's access into the clone. Revoke them in the \
         source (REVOKE, DROP ROLE) and clone again",
        found.join("; ")
    )
}

/// The statement that reads every account's authentication state after the
/// identity was set: `user host plugin locked expired require_current ssl
/// secondary`, tab-separated, with no empty field.
pub const AUTH_STATE_SQL: &str = "\
SELECT user, host, plugin, account_locked, password_expired, IFNULL(password_require_current, '-'), \
IF(ssl_type = '', 'NONE', ssl_type), \
IF(JSON_EXTRACT(User_attributes, '$.additional_password') IS NULL, 'none', 'SECONDARY') \
FROM mysql.user ORDER BY user, host;\n";

/// Judge [`AUTH_STATE_SQL`]'s rows after the identity was set: a branch's two
/// accounts are exactly what [`AUTH_OPTIONS`] asks for, MySQL's own are locked,
/// and no account anywhere has a secondary password, which would still open the
/// clone with a password its source knew. Refuses (naming accounts, never a
/// secret) when anything is otherwise.
pub fn require_normalized_authentication(rows: &str, app_user: &str) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();
    for line in rows.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split('\t').collect();
        let [
            user,
            host,
            plugin,
            locked,
            expired,
            require_current,
            ssl,
            secondary,
        ] = fields.as_slice()
        else {
            problems.push(format!("an unreadable row ({} fields)", fields.len()));
            continue;
        };
        let name = format!("'{user}'@'{host}'");
        if *secondary != "none" {
            problems.push(format!("{name} still has a secondary password"));
        }
        let managed = (*user, *host) == ("root", "localhost") || (*user, *host) == (app_user, "%");
        if managed {
            if *plugin != "caching_sha2_password" {
                problems.push(format!("{name} uses the {plugin} plugin"));
            }
            if *locked != "N" {
                problems.push(format!("{name} is locked"));
            }
            if *expired != "N" {
                problems.push(format!("{name} has an expired password"));
            }
            if !matches!(*require_current, "-" | "N") {
                problems.push(format!("{name} requires the current password to change it"));
            }
            if *ssl != "NONE" {
                problems.push(format!("{name} requires TLS credentials ({ssl})"));
            }
        } else if SYSTEM_ACCOUNTS.contains(&(*user, *host)) {
            if *locked != "Y" {
                problems.push(format!("{name} is not locked"));
            }
        } else {
            problems.push(format!("{name} is an account devflow did not create"));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    bail!(
        "after the identity was set the accounts are not as devflow requires: {}. The clone is \
         abandoned so that no password of its source can open it",
        problems.join("; ")
    )
}

/// Refuse anything but the profile's version: the data directory format and
/// the shutdown report devflow relies on are MySQL 8.4's.
pub fn check_version(version: &str) -> Result<()> {
    if version.trim().starts_with("8.4.") {
        Ok(())
    } else {
        bail!(
            "the server is MySQL {}, and devflow's profile ({PROFILE}) is for MySQL 8.4: other \
             versions need their own recovery and invariant tests before they can be cloned",
            version.trim()
        )
    }
}

/// Fail with the accounts that make a data directory unfit to be a branch.
pub fn require_no_extra_accounts(rows: &str, app_user: &str) -> Result<()> {
    let extras = extra_accounts(rows, app_user);
    if extras.is_empty() {
        return Ok(());
    }
    bail!(
        "the data being cloned has accounts devflow did not create: {}. A branch has only \
         MySQL's own accounts, root@localhost and the application account, because every other \
         account would keep its source's password in the clone. Remove them in the source \
         (DROP USER) and clone again",
        extras.join(", ")
    )
}

/// The user tables of a running source that are not InnoDB, outside MySQL's
/// own schemas (`mysql` keeps CSV log tables). Base tables only: a view has no
/// engine. `schema<TAB>table<TAB>engine` per row.
pub const NON_INNODB_SQL: &str = "\
SELECT table_schema, table_name, IFNULL(engine, '?') FROM information_schema.tables \
WHERE table_type = 'BASE TABLE' \
AND table_schema NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys') \
AND (engine IS NULL OR UPPER(engine) <> 'INNODB') \
ORDER BY table_schema, table_name;\n";

/// Refuse a source that has tables outside InnoDB, naming them. A clone stops
/// the source, and a MEMORY table loses its rows when the server does; the
/// other engines are outside the cloning contract. `rows` is the output of
/// [`NON_INNODB_SQL`].
pub fn require_only_innodb(rows: &str) -> Result<()> {
    const LISTED: usize = 10;
    let tables: Vec<String> = rows
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let schema = fields.next().unwrap_or("");
            let table = fields.next().unwrap_or("");
            let engine = match fields.next().unwrap_or("?") {
                "?" => "unknown engine",
                engine => engine,
            };
            format!("{}.{} ({engine})", quote_ident(schema), quote_ident(table))
        })
        .collect();
    if tables.is_empty() {
        return Ok(());
    }
    let more = tables.len().saturating_sub(LISTED);
    let mut listed = tables[..tables.len().min(LISTED)].join(", ");
    if more > 0 {
        listed.push_str(&format!(" and {more} more"));
    }
    bail!(
        "the source has tables that are not InnoDB: {listed}. A clone shuts the server down, a \
         MEMORY table loses its rows when it does, and devflow's MySQL profile supports InnoDB \
         only. Convert them (ALTER TABLE … ENGINE=InnoDB) or drop them and clone again; the \
         source was not touched"
    )
}

/// Makes the next shutdown a slow one, whatever a session or `SET PERSIST`
/// said: the persisted entry (which a restart would apply over the profile) is
/// reset, the running value set, and the value read back.
pub const SLOW_SHUTDOWN_SQL: &str = "\
RESET PERSIST IF EXISTS innodb_fast_shutdown;\n\
SET GLOBAL innodb_fast_shutdown = 0;\n\
SELECT @@GLOBAL.innodb_fast_shutdown;\n";

/// Refuse unless [`SLOW_SHUTDOWN_SQL`] read back `0`.
pub fn require_slow_shutdown(output: &str) -> Result<()> {
    let mode = output.trim();
    anyhow::ensure!(
        mode == "0",
        "innodb_fast_shutdown could not be set to 0 before the stop (it reads {}), so the \
         shutdown would not leave a data directory that needs no recovery; the source was not \
         touched",
        if mode.is_empty() { "nothing" } else { mode }
    );
    Ok(())
}

/// The port a MySQL container listens on, for the OrbStack name.
pub fn container_endpoint(domain: &str) -> String {
    format!("{domain}:{CONTAINER_PORT}")
}

/// A database name and user are recorded with the data; this reads them
/// back from a parsed URL.
pub fn identity_of_url(url: &str) -> Result<(String, String)> {
    let parts = parse_url(url).context("not a mysql:// URL")?;
    Ok((parts.database, parts.user))
}

// ---- the copy helper ------------------------------------------------------

/// The script of the copy helper for a MySQL data directory: the whole
/// directory, reflinked (`--reflink=always`, never `auto`) from the read-only
/// source into the destination volume. `ALLOW_COPY` is the journaled
/// decision; `FIXUP` removes what makes a directory the source's identity
/// (server UUID, TLS and RSA key pairs: the server makes new ones when it
/// first starts); `DELAY` and `NO_REFLINK` are test hooks.
pub const COPY_SCRIPT: &str = r#"set -eu
free() { df -Pk /dst | awk 'NR==2 {print $4}'; }
before=$(free)
if [ ! -d /src/mysql ] || [ ! -s /src/ibdata1 ]; then echo "result=empty"; exit 0; fi
size=$(du -sb /src | cut -f1)
mtime=$(stat -c %Y /src/ibdata1)
if [ -n "${DELAY:-}" ]; then sleep "$DELAY"; fi
cloned=no
if [ -n "${NO_REFLINK:-}" ]; then
  echo "reflink disabled for this run (test hook): Operation not supported" > /tmp/cp.err
elif cp -a --reflink=always /src/. /dst/ 2>/tmp/cp.err; then
  cloned=yes
fi
if [ "$cloned" = yes ]; then
  method=reflink
elif [ "${ALLOW_COPY:-0}" = 1 ]; then
  find /dst -mindepth 1 -delete
  cp -a /src/. /dst/
  method=copy
else
  echo "result=refused error=$(tr '\n' ' ' < /tmp/cp.err)"
  exit 3
fi
if [ -n "${FIXUP:-}" ]; then
  rm -f /dst/auto.cnf /dst/ca.pem /dst/ca-key.pem /dst/server-cert.pem /dst/server-key.pem \
    /dst/client-cert.pem /dst/client-key.pem /dst/private_key.pem /dst/public_key.pem
fi
sync
after=$(free)
echo "result=ok method=$method size=$size mtime=$mtime before=$before after=$after"
"#;

// ---- running SQL inside a container ----------------------------------------

/// Runs `mysql` with the first line of its standard input as the password,
/// set in the client's environment only (`MYSQL_PWD`): the password is never
/// on a command line, in the environment of `docker exec`, or in a file.
const AUTH_SCRIPT: &str = "IFS= read -r PW || exit 70\nMYSQL_PWD=\"$PW\" exec mysql \"$@\"\n";

/// The login of a branch's application account.
#[derive(Debug, Clone)]
pub struct Login {
    pub user: String,
    pub password: String,
    pub database: String,
}

/// Mask the passwords in text that came back from a container.
fn scrub(text: &str, secrets: &[&str]) -> String {
    crate::redact::text_with(text.trim(), secrets)
}

/// How a failed attempt to log in reads.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The server answered and refused the credentials: waiting will not help.
    Refused(String),
    /// Not up yet (or not reachable yet): try again until the deadline.
    NotYet(String),
}

fn classify_client_error(stderr: &str) -> Failure {
    let line = stderr
        .lines()
        .find(|l| l.contains("ERROR"))
        .or_else(|| stderr.lines().next())
        .unwrap_or("no output")
        .trim()
        .to_string();
    // ERROR 1045 (28000): Access denied for user 'app'@'localhost'
    if line.contains("1045") || line.contains("Access denied") {
        Failure::Refused(line)
    } else {
        Failure::NotYet(line)
    }
}

/// Whether the first bytes a server sent are a MySQL packet: the 4-byte
/// header, then protocol version 10 followed by the server version (the
/// greeting) or 255 (an error such as "too many connections", which only a
/// MySQL server sends first). The version's first digit keeps another
/// protocol's reply that happens to have a newline there from passing.
fn is_mysql_greeting(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && ((bytes[4] == 10 && bytes[5].is_ascii_digit()) || bytes[4] == 0xff)
}

/// Read the greeting a MySQL server sends as soon as a client connects to
/// the published port: it proves the endpoint reaches a MySQL server (and
/// not whatever else holds that port), with no credentials involved.
fn greeting(port: u16) -> Result<()> {
    use std::io::Read;
    use std::net::{SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .with_context(|| format!("cannot connect to 127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut head = [0u8; 6];
    stream
        .read_exact(&mut head)
        .with_context(|| format!("127.0.0.1:{port} sent no MySQL greeting"))?;
    anyhow::ensure!(
        is_mysql_greeting(&head),
        "127.0.0.1:{port} answered, but not like a MySQL server"
    );
    Ok(())
}

/// Run `statements` as `login` inside `container` (over TCP on the loopback
/// of the container, or `extra` such as `--protocol=socket`).
fn run_as(
    container: &str,
    login: &Login,
    extra: &[&str],
    statements: &str,
    timeout: Duration,
) -> Result<docker::ExecOutput> {
    let mut args = vec![
        "bash",
        "-c",
        AUTH_SCRIPT,
        "devflow-mysql",
        "-u",
        &login.user,
        "--batch",
        "--skip-column-names",
    ];
    args.extend_from_slice(extra);
    args.push(&login.database);
    docker::exec_stdin(
        container,
        &args,
        &format!("{}\n{statements}", login.password),
        timeout,
    )
}

/// Run `statements` as `root` through the socket of a bootstrap server,
/// which has no grant tables: no password is asked, none is sent.
fn run_bootstrap_sql(container: &str, statements: &str, timeout: Duration) -> Result<String> {
    let out = docker::exec_stdin(
        container,
        &[
            "mysql",
            "-uroot",
            "--protocol=socket",
            "--batch",
            "--skip-column-names",
        ],
        statements,
        timeout,
    )?;
    anyhow::ensure!(
        out.code == 0,
        "the MySQL client exited with status {}: {}",
        out.code,
        out.stderr.trim()
    );
    Ok(out.stdout)
}

/// Wait until the application account's `SELECT 1` works inside the
/// container and the published port greets like MySQL. A wrong password is
/// final; a server still starting is waited for; a container that stopped
/// ends the wait at once.
pub fn health(container: &str, port: u16, login: &Login, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let secrets = [login.password.as_str()];
    loop {
        match docker::inspect_container(container)? {
            Some(info) if info.running => {}
            Some(info) => bail!(
                "the MySQL container {} is not running (status {}, exit code {})",
                info.name,
                info.status,
                info.exit_code
            ),
            None => bail!("the MySQL container {container} no longer exists"),
        }
        let why = match run_as(
            container,
            login,
            &[],
            "SELECT 1;\n",
            Duration::from_secs(30),
        ) {
            Ok(out) if out.code == 0 && out.stdout.trim() == "1" => match greeting(port) {
                Ok(()) => return Ok(()),
                Err(e) => format!("{e:#}"),
            },
            Ok(out) => match classify_client_error(&scrub(&out.stderr, &secrets)) {
                Failure::Refused(why) => {
                    bail!("the server rejected the application account's own password: {why}")
                }
                Failure::NotYet(why) => why,
            },
            Err(e) => scrub(&format!("{e:#}"), &secrets),
        };
        if Instant::now() >= deadline {
            bail!(
                "the MySQL server on 127.0.0.1:{port} did not become healthy within {}: \
                 {why}",
                crate::text::counted(timeout.as_secs(), "second")
            );
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

// ---- a clean stop, proven ----------------------------------------------------

/// InnoDB's report that it closed everything and wrote its last checkpoint,
/// and the server's own report that it is done. Both are in the log of a
/// shutdown that ran to the end (`log_error_verbosity=3`).
const INNODB_SHUTDOWN_DONE: &str = "Shutdown completed; log sequence number";
const SERVER_SHUTDOWN_DONE: &str = "Shutdown complete (mysqld";
/// What InnoDB logs (MY-013078) when it was asked for `innodb_fast_shutdown=2`:
/// it stops without flushing its buffer pool and "at the next mysqld startup
/// InnoDB will do a crash recovery". Both shutdown reports above are logged
/// all the same, so this notice is what tells a crash-style stop apart.
const VERY_FAST_SHUTDOWN: &str = "requested a very fast shutdown";

/// What InnoDB logs (MY-012552) when a start has a redo log to apply. A start
/// after a clean shutdown ("Applying a batch of 0 redo log records", and the
/// server's "Crash recovery finished" note, which is logged every time) does
/// not log it.
const CRASH_RECOVERY: &str = "Starting crash recovery";

/// The line of `logs` that says InnoDB began crash recovery, if any: the data
/// directory it started from was not left by a clean shutdown.
pub fn crash_recovery_line(logs: &str) -> Option<&str> {
    logs.lines().find(|line| line.contains(CRASH_RECOVERY))
}

/// Judge a stopped container: it must have exited with status 0 (a
/// `SIGKILL` after the stop timeout is 137) and its log since it started
/// must hold both shutdown reports and no notice of a very fast shutdown
/// (`innodb_fast_shutdown=2`, which exits 0 and logs both reports and leaves a
/// data directory that needs crash recovery). None alone is enough: an exit
/// code 0 does not say InnoDB finished, and a log line does not say the
/// process was not killed after it.
pub fn judge_stop(running: bool, exit_code: i64, logs: &str) -> Result<()> {
    anyhow::ensure!(!running, "the container is still running");
    anyhow::ensure!(
        exit_code == 0,
        "the server exited with status {exit_code} instead of 0 ({}): it did not shut down \
         cleanly, so its data directory is not safe to copy",
        if exit_code == 137 {
            "137 is a SIGKILL after the stop timeout"
        } else {
            "it was killed or crashed"
        }
    );
    anyhow::ensure!(
        logs.contains(INNODB_SHUTDOWN_DONE) && logs.contains(SERVER_SHUTDOWN_DONE),
        "the server exited with status 0 but its log holds no InnoDB shutdown report since it \
         started, so devflow cannot tell that the shutdown ran to the end"
    );
    anyhow::ensure!(
        !logs.contains(VERY_FAST_SHUTDOWN),
        "the server was shut down with innodb_fast_shutdown=2, which exits 0 but leaves a data \
         directory that needs crash recovery, so it is not a clean shutdown to copy. Start the \
         source and clone again: devflow sets innodb_fast_shutdown=0 before it stops a running \
         source"
    );
    Ok(())
}

/// Inspect a stopped container and judge its last shutdown ([`judge_stop`]).
pub fn verify_stopped(container: &str) -> Result<ContainerInfo> {
    let info = docker::inspect_container(container)?
        .with_context(|| format!("container {container} no longer exists"))?;
    let logs = if info.running {
        String::new()
    } else {
        docker::logs_since(&info.id, &info.started_at)?
    };
    judge_stop(info.running, info.exit_code, &logs)?;
    Ok(info)
}

/// Stop `container` and prove the shutdown was clean.
pub fn stop_clean(container: &str, timeout: Duration) -> Result<()> {
    docker::stop_container(container, timeout)?;
    verify_stopped(container).map(|_| ())
}

/// When a stopped container's process exited, from its inspect data.
fn finished_at(info: &ContainerInfo) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&info.finished_at)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

// ---- the bootstrap ---------------------------------------------------------------

/// The command of a bootstrap container: take ownership of the data
/// directory, initialize it if it is empty and `init` allows, then run a
/// server with no network and no grant tables. Run by `bash -c`, which the
/// image's entrypoint passes through untouched.
pub fn bootstrap_script(init: bool) -> String {
    let missing = if init {
        "if [ -n \"$(ls -A \"$D\")\" ]; then \
         echo \"devflow: $D is not empty but holds no MySQL data directory\" >&2; exit 78; fi\n  \
         gosu mysql mysqld --initialize-insecure --default-time-zone=SYSTEM"
    } else {
        "echo \"devflow: $D holds no MySQL data directory to start from\" >&2; exit 78"
    };
    format!(
        "set -eu\n\
         D={DATA_DIR}\n\
         chown mysql:mysql \"$D\"\n\
         if [ ! -d \"$D/mysql\" ]; then\n  {missing}\nfi\n\
         exec gosu mysql mysqld --skip-networking --skip-grant-tables\n"
    )
}

/// What a bootstrap is run on.
pub(super) struct Boot<'a> {
    /// The branch whose identity is set: its database, account and passwords.
    pub branch: &'a BranchRecord,
    pub name: &'a str,
    pub volume: &'a str,
    pub pin: &'a str,
    /// The volume is new and empty (a root, or a reset to empty): the
    /// server is initialized first. Otherwise it must hold a data directory.
    pub init: bool,
}

impl Service {
    /// The login of `record`'s application account.
    pub(super) fn mysql_login(&self, record: &BranchRecord) -> Result<Login> {
        let identity = record
            .mysql
            .as_ref()
            .with_context(|| format!("branch {} records no database and account", record.id))?;
        Ok(Login {
            user: identity.user.clone(),
            password: record.password.clone(),
            database: identity.database.clone(),
        })
    }

    /// The path of the profile file on the host, written if it is missing
    /// or different (it is the same for every branch and holds no secret,
    /// so it is not private: the container's `mysql` user reads it).
    pub(super) fn mysql_profile_file(&self) -> Result<String> {
        let path = crate::paths::data_dir()?
            .join("services")
            .join(PROFILE_FILE);
        let current = std::fs::read(&path).ok();
        if current.as_deref() != Some(CNF.as_bytes()) {
            crate::fsutil::write_atomic(&path, CNF.as_bytes(), false)?;
        }
        let text = path
            .to_str()
            .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
        // `--mount` takes comma-separated fields.
        anyhow::ensure!(
            !text.contains([',', '"', '\n']),
            "the data directory {} has a comma, a quote or a newline in its path, which Docker's \
             --mount cannot take; set DEVFLOW_DATA_DIR to another place",
            path.display()
        );
        Ok(text.to_string())
    }

    /// The login of `record`'s `root@localhost`, which exists inside the
    /// container only and is reached through its socket.
    fn mysql_root_login(&self, record: &BranchRecord) -> Result<Login> {
        Ok(Login {
            user: "root".into(),
            password: record
                .root_password
                .clone()
                .with_context(|| format!("branch {} records no root password", record.id))?,
            database: "mysql".into(),
        })
    }

    /// Run `sql` as root through the socket of the running source `container`.
    fn mysql_root_sql(&self, record: &BranchRecord, container: &str, sql: &str) -> Result<String> {
        let root = self.mysql_root_login(record)?;
        let out = run_as(
            container,
            &root,
            &["--protocol=socket"],
            sql,
            Duration::from_secs(60),
        )?;
        anyhow::ensure!(
            out.code == 0,
            "the MySQL client exited with status {}: {}",
            out.code,
            scrub(&out.stderr, &[root.password.as_str()])
        );
        Ok(out.stdout)
    }

    /// What a running source must satisfy before it is stopped for a clone,
    /// checked while it still serves, so that a refusal touches nothing.
    fn mysql_preflight(&self, record: &BranchRecord, container: &str) -> Result<()> {
        let tables = self.mysql_root_sql(record, container, NON_INNODB_SQL)?;
        require_only_innodb(&tables)?;
        // The shutdown must leave nothing to recover: the profile says
        // `innodb_fast_shutdown=0`, but a session (or `SET PERSIST`, which a
        // restart would apply over the profile) may have said otherwise. Set
        // right before the stop, and read back.
        let mode = self.mysql_root_sql(record, container, SLOW_SHUTDOWN_SQL)?;
        require_slow_shutdown(&mode)
    }

    /// Stop a running source cleanly and prove it, or find a stopped one
    /// that was. Returns the stop to undo (`None` when the source was not
    /// running) and when the server finished shutting down: the instant of
    /// the data. The stop is recorded as intent first by the caller
    /// (`source_running`), so a coordinator that dies here is recovered.
    pub(super) fn mysql_quiesce(
        &self,
        op_id: Uuid,
        branch: &BranchRecord,
        running: bool,
        stop_timeout_secs: u64,
    ) -> Result<(Option<Stopped>, DateTime<Utc>)> {
        let info = self.find_container(branch)?.ok_or_else(|| {
            self.start_problem(
                format!(
                    "branch {} has no container, so devflow cannot tell that its data directory \
                     was shut down cleanly; start it and clone again",
                    branch.id
                ),
                Some(branch.id),
            )
            .into_error()
        })?;
        if running {
            // Refused here, while the source still serves, if it cannot be
            // stopped without losing anything.
            self.mysql_preflight(branch, &info.id)?;
            // The downtime starts when the stop is asked for.
            let since = Instant::now();
            // Journaled before it is issued: the client can outlive this
            // process, and recovery fences the container before restarting the source.
            self.stop_journaled(op_id, &info, Duration::from_secs(stop_timeout_secs))?;
            let stopped = verify_stopped(&info.id).context(
                "the source did not shut down cleanly; the clone is abandoned and the source \
                 is started again",
            )?;
            Ok((
                Some(Stopped {
                    source: branch.id,
                    since,
                }),
                finished_at(&stopped),
            ))
        } else {
            let stopped = verify_stopped(&info.id).map_err(|error| {
                error.context(self.start_problem(
                    "the source is stopped, but not known to have shut down cleanly; start it \
                     and clone again",
                    Some(branch.id),
                ))
            })?;
            Ok((None, finished_at(&stopped)))
        }
    }

    /// After the source was started again following its clean stop: its log
    /// since that start must not say InnoDB did crash recovery. If it did,
    /// the stop proof was wrong (a session asked for a very fast shutdown in
    /// the instant between the preflight and the stop), the copy is of a
    /// directory that needed recovery, and the clone is abandoned.
    pub(super) fn mysql_require_clean_start(&self, branch: Uuid) -> Result<()> {
        let record = self.require_branch(branch)?;
        let info = self
            .find_container(&record)?
            .context("the source has no container after it was started again")?;
        let logs = docker::logs_since(&info.id, &info.started_at)?;
        if let Some(line) = crash_recovery_line(&logs) {
            bail!(
                "the source ran InnoDB crash recovery when it started again ({}), so its \
                 shutdown was not clean and the copy taken from it is not either; the clone is \
                 abandoned",
                crate::redact::text(line.trim())
            );
        }
        Ok(())
    }

    /// Start a source an unfinished operation stopped. Idempotent: a source
    /// that runs is only checked. Run by recovery and by a rollback, after the
    /// copy helper is gone.
    pub(super) fn restore_stopped_source(&self, op: &Op) -> Result<()> {
        if self.engine != Engine::Mysql || op.source_running != Some(true) {
            return Ok(());
        }
        let Some(SourceRef::Branch { id }) = op.source else {
            return Ok(());
        };
        self.restart_after_stop(id, op)
            .context("the source branch could not be started again")?;
        Ok(())
    }

    /// Bootstrap the destination of operation `op`: the branch it creates.
    pub(super) fn mysql_bootstrap_branch(&self, op: &Op, init: bool) -> Result<()> {
        let branch = self.require_branch(op.target)?;
        let name = op
            .boot_name
            .clone()
            .context("no bootstrap container name was journaled")?;
        let op_id = op.id;
        self.mysql_bootstrap(
            &Boot {
                branch: &branch,
                name: &name,
                volume: &branch.volume,
                pin: &branch.pin,
                init,
            },
            &mut |id| {
                self.update(|service| {
                    if let Some(entry) = service.op_mut(op_id) {
                        entry.boot_id = id;
                    }
                    Ok(())
                })
            },
        )
    }

    /// Bootstrap a reset's replacement volume with the branch's own
    /// credentials (a reset keeps the URL), in the container the plan names
    /// for staging.
    pub(super) fn mysql_stage_reset(
        &self,
        op: &Op,
        branch: &BranchRecord,
        plan: &ResetPlan,
        init: bool,
    ) -> Result<()> {
        let op_id = op.id;
        self.mysql_bootstrap(
            &Boot {
                branch,
                name: &plan.staging_name,
                volume: &plan.volume,
                pin: &plan.pin,
                init,
            },
            &mut |id| {
                self.update(|service| {
                    if let Some(plan) = service.op_mut(op_id).and_then(|entry| entry.reset.as_mut())
                    {
                        plan.staging_id = id;
                    }
                    Ok(())
                })
            },
        )
    }

    /// Give a data directory its branch identity before it ever listens on a
    /// network. A throwaway container with no network and no grant tables
    /// runs the server; its local socket (`docker exec`) is the only way in.
    /// The version is checked, accounts the branch should not have are
    /// refused, the passwords and schema are set, both logins are proven, and
    /// the server is shut down cleanly and removed. The container's id is
    /// journaled (`record_id`) before it starts, and cleared when it is gone.
    pub(super) fn mysql_bootstrap(
        &self,
        boot: &Boot<'_>,
        record_id: &mut dyn FnMut(Option<String>) -> Result<()>,
    ) -> Result<()> {
        let identity = boot
            .branch
            .mysql
            .as_ref()
            .context("the branch records no database and account")?;
        let root_password = boot
            .branch
            .root_password
            .as_deref()
            .context("the branch records no root password")?;
        let secrets = [boot.branch.password.as_str(), root_password];
        let owner = VolumeOwner::Branch(boot.branch.id);
        if boot.init {
            self.ensure_volume(boot.volume, Some(owner))?;
        } else {
            self.require_source_volume(boot.volume, owner)?;
        }
        let mut labels = self.labels(boot.branch.id);
        labels.push(("dev.devflow.staging".into(), "1".into()));
        let id = docker::create_container(&ContainerSpec {
            name: boot.name.to_string(),
            image: boot.pin.to_string(),
            volume: boot.volume.to_string(),
            data_dir: DATA_DIR.to_string(),
            host_port: None,
            container_port: CONTAINER_PORT,
            isolated: true,
            files: vec![(self.mysql_profile_file()?, PROFILE_MOUNT.to_string())],
            labels,
            command: vec!["bash".into(), "-c".into(), bootstrap_script(boot.init)],
        })?;
        // Recorded before it starts: whoever recovers this removes it.
        record_id(Some(id.clone()))?;
        docker::start_container(&id).map_err(|e| match e {
            docker::StartError::Bind(why) => anyhow::anyhow!("{why}"),
            docker::StartError::Other(e) => e,
        })?;

        let work = || -> Result<()> {
            self.mysql_wait_socket(&id, Duration::from_secs(300))?;
            // A data directory copied from a clean shutdown starts without
            // crash recovery. One that does recover was not: whatever the
            // proof said, this clone is not made from it.
            if !boot.init {
                let started = docker::inspect_container(&id)?
                    .context("the bootstrap container vanished")?
                    .started_at;
                if let Some(line) = crash_recovery_line(&docker::logs_since(&id, &started)?) {
                    bail!(
                        "the data directory being cloned needed InnoDB crash recovery when the \
                         clone first started ({}), so it was not copied from a clean shutdown; \
                         the clone is abandoned",
                        crate::redact::text(line.trim())
                    );
                }
            }
            let version = run_bootstrap_sql(&id, "SELECT @@version;\n", Duration::from_secs(60))?;
            check_version(&version)?;
            let accounts = run_bootstrap_sql(
                &id,
                "SELECT user, host FROM mysql.user ORDER BY user, host;\n",
                Duration::from_secs(60),
            )?;
            require_no_extra_accounts(&accounts, &identity.user)?;
            let graph = run_bootstrap_sql(&id, GRAPH_AUDIT_SQL, Duration::from_secs(60))?;
            require_no_roles_or_proxies(&graph)?;
            let sql = identity_sql(&Identity {
                database: &identity.database,
                user: &identity.user,
                app_password: &boot.branch.password,
                root_password,
            });
            run_bootstrap_sql(&id, &sql, Duration::from_secs(60))
                .map_err(|e| anyhow::anyhow!(scrub(&format!("{e:#}"), &secrets)))?;
            // The grant tables are loaded now (`FLUSH PRIVILEGES` ends the
            // bootstrap server's skip-grant-tables mode): everything from here
            // on logs in with the new passwords, over the socket.
            let app = Login {
                user: identity.user.clone(),
                password: boot.branch.password.clone(),
                database: identity.database.clone(),
            };
            let root = Login {
                user: "root".into(),
                password: root_password.to_string(),
                database: identity.database.clone(),
            };
            // Nothing of the source's authentication is left: no secondary
            // password, nothing locked, expired or tied to a certificate.
            let state = run_as(
                &id,
                &root,
                &["--protocol=socket"],
                AUTH_STATE_SQL,
                Duration::from_secs(60),
            )?;
            anyhow::ensure!(
                state.code == 0,
                "the accounts could not be read back with the new root password: {}",
                scrub(&state.stderr, &secrets)
            );
            require_normalized_authentication(&state.stdout, &identity.user)?;
            // The new passwords work; nothing was assumed.
            for login in [&app, &root] {
                let out = run_as(
                    &id,
                    login,
                    &["--protocol=socket"],
                    "SELECT 1;\n",
                    Duration::from_secs(60),
                )?;
                anyhow::ensure!(
                    out.code == 0 && out.stdout.trim() == "1",
                    "the {} login set during the bootstrap does not work: {}",
                    login.user,
                    scrub(&out.stderr, &secrets)
                );
            }
            stop_clean(&id, Duration::from_secs(300))
                .context("the bootstrap server did not shut down cleanly")
        };
        if let Err(error) = work() {
            let logs = docker::logs_tail(&id, 25);
            return Err(error.context(format!(
                "bootstrap of {} failed; its log:\n{logs}",
                boot.name
            )));
        }
        docker::kill_and_confirm(&id, Duration::from_secs(20))?;
        record_id(None)
    }

    /// Wait until the bootstrap server answers through its socket. A
    /// container that exited (the data directory could not be initialized or
    /// opened) ends the wait with its log.
    fn mysql_wait_socket(&self, container: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let info = docker::inspect_container(container)?
                .context("the bootstrap container vanished")?;
            anyhow::ensure!(
                info.running,
                "the bootstrap container exited with status {} before the server answered",
                info.exit_code
            );
            let ping = docker::exec_stdin(
                container,
                &[
                    "mysql",
                    "-uroot",
                    "--protocol=socket",
                    "--batch",
                    "--skip-column-names",
                ],
                "SELECT 1;\n",
                Duration::from_secs(30),
            );
            if matches!(&ping, Ok(out) if out.code == 0 && out.stdout.trim() == "1") {
                return Ok(());
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "the bootstrap server did not answer within {}",
                crate::text::counted(timeout.as_secs(), "second")
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_carry_every_piece_encoded_and_read_back() {
        // kingfisher:ignore
        let text = url("app", "p@ss/w:rd", "127.0.0.1", 23456, "my db");
        assert_eq!(
            text,
            "mysql://app:p%40ss%2Fw%3Ard@127.0.0.1:23456/my%20db" // kingfisher:ignore
        );
        let parts = parse_url(&text).unwrap();
        assert_eq!(
            parts,
            UrlParts {
                user: "app".into(),
                password: "p@ss/w:rd".into(),
                host: "127.0.0.1".into(),
                port: 23456,
                database: "my db".into()
            }
        );
        assert!(parse_url("redis://:pw@127.0.0.1:1/0").is_none()); // kingfisher:ignore
        assert!(parse_url("mysql://app@127.0.0.1:1/db").is_none());
        assert!(parse_url("mysql://app:pw@127.0.0.1/db").is_none()); // kingfisher:ignore
        assert!(parse_url("mysql://app:pw@127.0.0.1:x/db").is_none()); // kingfisher:ignore
        // Options after the database are not part of its name.
        assert_eq!(
            parse_url("mysql://a:b@h:1/db?ssl-mode=DISABLED") // kingfisher:ignore
                .unwrap()
                .database,
            "db"
        );
    }

    #[test]
    fn env_fields_follow_the_url() {
        let fields = env_fields("mysql://app:secret@127.0.0.1:23456/shop").unwrap(); // kingfisher:ignore
        let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, crate::config::MYSQL_FIELD_ENVS);
        assert_eq!(fields[0].1, "127.0.0.1");
        assert_eq!(fields[1].1, "23456");
        assert_eq!(fields[2].1, "app");
        assert_eq!(fields[3].1, "secret"); // kingfisher:ignore
        assert_eq!(fields[4].1, "shop");
        assert!(env_fields("not a url").is_none());
    }

    #[test]
    fn sql_literals_and_identifiers_cannot_be_escaped_out_of() {
        assert_eq!(quote_str("plain"), "'plain'");
        assert_eq!(quote_str("it's"), "'it''s'");
        assert_eq!(quote_str(r"a\'b"), r"'a\\''b'");
        assert_eq!(quote_str("nul\0"), r"'nul\0'");
        assert_eq!(quote_ident("shop"), "`shop`");
        assert_eq!(quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn the_identity_statements_set_both_passwords_and_the_schema_idempotently() {
        let sql = identity_sql(&Identity {
            database: "shop",
            user: "app",
            app_password: "apppw",   // kingfisher:ignore
            root_password: "rootpw", // kingfisher:ignore
        });
        let lines: Vec<&str> = sql.lines().collect();
        assert_eq!(lines[0], "FLUSH PRIVILEGES;", "grants are loaded first");
        assert_eq!(*lines.last().unwrap(), "FLUSH PRIVILEGES;");
        // The password is set with the plugin named and every setting a source
        // could have changed put back.
        let altered = |account: &str, password: &str| {
            format!(
                "ALTER USER {account} IDENTIFIED WITH caching_sha2_password BY '{password}' \
                 {AUTH_OPTIONS};"
            )
        };
        assert!(lines.contains(&altered("'root'@'localhost'", "rootpw").as_str()));
        assert!(lines.contains(&altered("'app'@'%'", "apppw").as_str()));
        assert!(lines.contains(&"CREATE DATABASE IF NOT EXISTS `shop`;"));
        assert!(lines.contains(
            &"CREATE USER IF NOT EXISTS 'app'@'%' IDENTIFIED WITH caching_sha2_password BY 'apppw';"
        ));
        assert!(lines.contains(&"GRANT ALL PRIVILEGES ON `shop`.* TO 'app'@'%';"));
        // A secondary password (`RETAIN CURRENT PASSWORD`) survives a plain
        // `IDENTIFIED BY`: it is discarded by its own statement, after the
        // new password is set, for both accounts.
        for account in ["'root'@'localhost'", "'app'@'%'"] {
            let alter = lines
                .iter()
                .position(|l| l.starts_with(&format!("ALTER USER {account} IDENTIFIED WITH")))
                .unwrap();
            let discard = lines
                .iter()
                .position(|l| *l == format!("ALTER USER {account} DISCARD OLD PASSWORD;"))
                .unwrap_or_else(|| panic!("{account} has its secondary password discarded"));
            assert!(alter < discard, "{account}");
        }
        for (user, host) in SYSTEM_ACCOUNTS {
            assert!(sql.contains(&format!("ALTER USER '{user}'@'{host}' ACCOUNT LOCK;")));
            assert!(sql.contains(&format!(
                "ALTER USER '{user}'@'{host}' DISCARD OLD PASSWORD;"
            )));
        }
        for option in [
            "REQUIRE NONE",
            "PASSWORD EXPIRE NEVER",
            "PASSWORD REQUIRE CURRENT DEFAULT",
            "FAILED_LOGIN_ATTEMPTS 0",
            "PASSWORD_LOCK_TIME 0",
            "ACCOUNT UNLOCK",
        ] {
            assert!(AUTH_OPTIONS.contains(option), "{option}");
        }
        assert!(
            !sql.contains("'root'@'%'"),
            "root is local to the container"
        );
        // A hostile value stays inside its literal.
        let sql = identity_sql(&Identity {
            database: "shop",
            user: "app",
            app_password: "x'; DROP USER 'root'@'localhost'; --", // kingfisher:ignore
            root_password: "r",                                   // kingfisher:ignore
        });
        assert!(sql.contains(r"BY 'x''; DROP USER ''root''@''localhost''; --' REQUIRE NONE"));
    }

    #[test]
    fn roles_and_proxy_grants_beyond_the_defaults_refuse_the_clone() {
        // A fresh server: no role edge, no default role, root's own proxy row.
        assert!(require_no_roles_or_proxies("proxy\tlocalhost,root,,,1\n").is_ok());
        assert!(require_no_roles_or_proxies("").is_ok());
        let error = require_no_roles_or_proxies(
            "role_edge\tdev@% -> app@%\n\
             default_role\tapp@% -> dev@%\n\
             proxy\tlocalhost,root,,,1\n\
             proxy\t%,app,,admin,0\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("role grant dev@% -> app@%")
                && error.contains("default role app@% -> dev@%")
                && error.contains("proxy privilege (%,app,,admin,0)")
                && error.contains("REVOKE"),
            "{error}"
        );
        // The default proxy row is only the default when it is exactly so.
        assert!(require_no_roles_or_proxies("proxy\t%,root,,,1\n").is_err());
        assert!(require_no_roles_or_proxies("proxy\tlocalhost,root,,,0\n").is_err());
    }

    #[test]
    fn no_account_may_keep_a_secondary_password_or_a_setting_of_its_source() {
        let healthy = "app\t%\tcaching_sha2_password\tN\tN\t-\tNONE\tnone\n\
                       mysql.infoschema\tlocalhost\tcaching_sha2_password\tY\tN\t-\tNONE\tnone\n\
                       mysql.session\tlocalhost\tcaching_sha2_password\tY\tN\t-\tNONE\tnone\n\
                       mysql.sys\tlocalhost\tcaching_sha2_password\tY\tN\t-\tNONE\tnone\n\
                       root\tlocalhost\tcaching_sha2_password\tN\tN\tN\tNONE\tnone\n";
        assert!(require_normalized_authentication(healthy, "app").is_ok());
        // Each of these rows alone, in place of its healthy line, is refused.
        for (row, what) in [
            (
                "app\t%\tcaching_sha2_password\tN\tN\t-\tNONE\tSECONDARY",
                "'app'@'%' still has a secondary password",
            ),
            (
                "mysql.sys\tlocalhost\tcaching_sha2_password\tY\tN\t-\tNONE\tSECONDARY",
                "'mysql.sys'@'localhost' still has a secondary password",
            ),
            (
                "root\tlocalhost\tmysql_native_password\tN\tN\t-\tNONE\tnone",
                "uses the mysql_native_password plugin",
            ),
            (
                "app\t%\tcaching_sha2_password\tY\tN\t-\tNONE\tnone",
                "is locked",
            ),
            (
                "app\t%\tcaching_sha2_password\tN\tY\t-\tNONE\tnone",
                "has an expired password",
            ),
            (
                "app\t%\tcaching_sha2_password\tN\tN\tY\tNONE\tnone",
                "requires the current password",
            ),
            (
                "app\t%\tcaching_sha2_password\tN\tN\t-\tX509\tnone",
                "requires TLS credentials (X509)",
            ),
            (
                "mysql.session\tlocalhost\tcaching_sha2_password\tN\tN\t-\tNONE\tnone",
                "is not locked",
            ),
            (
                "reporting\t%\tcaching_sha2_password\tN\tN\t-\tNONE\tnone",
                "is an account devflow did not create",
            ),
            ("app\t%\tcaching_sha2_password", "an unreadable row"),
        ] {
            let error = require_normalized_authentication(row, "app")
                .unwrap_err()
                .to_string();
            assert!(error.contains(what), "{what}: {error}");
        }
    }

    #[test]
    fn only_mysqls_own_accounts_root_and_the_app_user_are_allowed() {
        let own = "mysql.infoschema\tlocalhost\nmysql.session\tlocalhost\nmysql.sys\tlocalhost\n\
                   root\tlocalhost\napp\t%\n";
        assert!(extra_accounts(own, "app").is_empty());
        assert!(require_no_extra_accounts(own, "app").is_ok());
        // The app account must be exactly app@%, root exactly root@localhost.
        let rows = "root\t%\nroot\tlocalhost\napp\tlocalhost\nreporting\t%\nmysql.sys\t%\n";
        assert_eq!(
            extra_accounts(rows, "app"),
            vec![
                "'app'@'localhost'",
                "'mysql.sys'@'%'",
                "'reporting'@'%'",
                "'root'@'%'"
            ]
        );
        let error = require_no_extra_accounts(rows, "app")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("'reporting'@'%'") && error.contains("DROP USER"),
            "{error}"
        );
        // A branch with a different application user name is refused too.
        assert_eq!(extra_accounts("app\t%\n", "other"), vec!["'app'@'%'"]);
    }

    #[test]
    fn only_mysql_8_4_has_a_profile() {
        assert!(check_version("8.4.11").is_ok());
        assert!(check_version("8.4.0\n").is_ok());
        for version in ["8.0.40", "9.1.0", "8.4", "5.7.44", "10.11.6-MariaDB"] {
            let error = check_version(version).unwrap_err().to_string();
            assert!(error.contains(version), "{error}");
        }
    }

    #[test]
    fn the_profile_is_standalone_small_and_shuts_down_slowly() {
        for setting in [
            "skip-log-bin",
            "mysqlx=OFF",
            "performance_schema=OFF",
            "innodb_buffer_pool_size=64M",
            "innodb_fast_shutdown=0",
            "bind-address=0.0.0.0",
            "log_error_verbosity=3",
            "default_storage_engine=InnoDB",
            "disabled_storage_engines=\"MyISAM,MEMORY,BLACKHOLE,ARCHIVE,CSV,FEDERATED,MERGE\"",
        ] {
            assert!(
                CNF.lines().any(|line| line == setting),
                "{setting} is in the profile"
            );
        }
        // Durability is left at MySQL's defaults.
        for weakened in [
            "innodb_flush_log_at_trx_commit",
            "innodb_doublewrite",
            "sync_binlog",
            "innodb_fast_shutdown=1",
            "innodb_fast_shutdown=2",
        ] {
            assert!(!CNF.contains(weakened), "{weakened}");
        }
        // No credentials and no replication in a file that is mounted for all.
        for forbidden in ["password", "server-id", "log-bin=", "replica", "master"] {
            assert!(!CNF.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn a_source_with_a_table_that_is_not_innodb_is_refused_naming_the_tables() {
        assert!(require_only_innodb("").is_ok());
        let error = require_only_innodb("shop\tcache\tMEMORY\nshop\tlog\tMyISAM\nother\tx\t?\n")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("`shop`.`cache` (MEMORY)")
                && error.contains("`shop`.`log` (MyISAM)")
                && error.contains("`other`.`x` (unknown engine)")
                && error.contains("lose")
                && error.contains("ENGINE=InnoDB")
                && error.contains("not touched"),
            "{error}"
        );
        // A long list is cut, with its length.
        let many: String = (0..25).map(|n| format!("s\tt{n}\tMEMORY\n")).collect();
        let error = require_only_innodb(&many).unwrap_err().to_string();
        assert!(
            error.contains("`s`.`t0`") && error.contains("and 15 more"),
            "{error}"
        );
        assert!(!error.contains("`s`.`t24`"), "{error}");
        // What the query asks for: base tables outside MySQL's own schemas.
        for schema in ["mysql", "information_schema", "performance_schema", "sys"] {
            assert!(NON_INNODB_SQL.contains(&format!("'{schema}'")), "{schema}");
        }
        assert!(
            NON_INNODB_SQL.contains("table_type = 'BASE TABLE'"),
            "views are excluded"
        );
    }

    #[test]
    fn a_stop_is_clean_only_with_exit_zero_and_both_shutdown_reports() {
        let logs = "x [Note] [MY-012980] [InnoDB] Shutdown completed; log sequence number 19387892\n\
                    y [System] [MY-010910] [Server] /usr/sbin/mysqld: Shutdown complete (mysqld 8.4.11)";
        assert!(judge_stop(false, 0, logs).is_ok());
        // A SIGKILL after the stop timeout.
        let error = judge_stop(false, 137, logs).unwrap_err().to_string();
        assert!(
            error.contains("137") && error.contains("SIGKILL"),
            "{error}"
        );
        assert!(judge_stop(false, 1, logs).is_err());
        // Exit 0 without the reports: nothing proves InnoDB finished.
        assert!(judge_stop(false, 0, "").is_err());
        assert!(judge_stop(false, 0, "[System] Shutdown complete").is_err());
        assert!(
            judge_stop(
                false,
                0,
                "[InnoDB] Shutdown completed; log sequence number 5"
            )
            .is_err()
        );
        // Still running is not stopped.
        assert!(judge_stop(true, 0, logs).is_err());
    }

    #[test]
    fn a_crash_style_shutdown_is_not_a_clean_one_even_with_both_reports_and_exit_zero() {
        // What 8.4.11 logs for a stop after `SET GLOBAL innodb_fast_shutdown=2`:
        // both reports are there, with the notice between them.
        let very_fast = "x [Note] [MY-013072] [InnoDB] Starting shutdown...\n\
             x [Note] [MY-013078] [InnoDB] MySQL has requested a very fast shutdown without \
             flushing the InnoDB buffer pool to data files. At the next mysqld startup InnoDB \
             will do a crash recovery!\n\
             x [Note] [MY-012980] [InnoDB] Shutdown completed; log sequence number 29687457\n\
             y [System] [MY-010910] [Server] /usr/sbin/mysqld: Shutdown complete (mysqld 8.4.11)";
        let error = judge_stop(false, 0, very_fast).unwrap_err().to_string();
        assert!(
            error.contains("innodb_fast_shutdown=2")
                && error.contains("crash recovery")
                && error.contains("Start the source and clone again"),
            "{error}"
        );
        // The slow shutdown of the profile logs no such notice.
        let slow = "x [Note] [MY-013072] [InnoDB] Starting shutdown...\n\
             x [Note] [MY-012980] [InnoDB] Shutdown completed; log sequence number 29600211\n\
             y [System] [MY-010910] [Server] /usr/sbin/mysqld: Shutdown complete (mysqld 8.4.11)";
        assert!(judge_stop(false, 0, slow).is_ok());
    }

    #[test]
    fn crash_recovery_is_told_from_the_notes_every_start_logs() {
        // A clean start of 8.4.11 logs these two, which are not recovery.
        let clean = "z [Note] [MY-012532] [InnoDB] Applying a batch of 0 redo log records ...\n\
             z [Note] [MY-013911] [Server] Crash recovery finished in InnoDB engine. No \
             attempts to commit, rollback or prepare any transactions.\n";
        assert_eq!(crash_recovery_line(clean), None);
        assert_eq!(crash_recovery_line(""), None);
        // A start with a redo log to apply says so first.
        let recovering = "z [Note] [MY-012560] [InnoDB] The log sequence number 1 in the system \
             tablespace does not match the log sequence number 2 in the redo log files!\n\
             z [Note] [MY-012552] [InnoDB] Starting crash recovery.\n\
             z [Note] [MY-012532] [InnoDB] Applying a batch of 10 redo log records ...\n";
        assert!(
            crash_recovery_line(recovering)
                .unwrap()
                .contains("Starting crash recovery")
        );
    }

    #[test]
    fn the_shutdown_is_made_slow_and_read_back_before_a_stop() {
        // Reset a persisted value first, then the running one, then read it.
        let lines: Vec<&str> = SLOW_SHUTDOWN_SQL.lines().collect();
        assert_eq!(
            lines,
            [
                "RESET PERSIST IF EXISTS innodb_fast_shutdown;",
                "SET GLOBAL innodb_fast_shutdown = 0;",
                "SELECT @@GLOBAL.innodb_fast_shutdown;"
            ]
        );
        assert!(require_slow_shutdown("0\n").is_ok());
        for read in ["2\n", "1", "", "NULL"] {
            let error = require_slow_shutdown(read).unwrap_err().to_string();
            assert!(error.contains("could not be set to 0"), "{read}: {error}");
        }
    }

    #[test]
    fn a_wrong_password_is_final_and_a_server_starting_is_waited_for() {
        assert!(matches!(
            classify_client_error(
                "mysql: [Warning] x\nERROR 1045 (28000): Access denied for user 'app'@'localhost'"
            ),
            Failure::Refused(_)
        ));
        for starting in [
            "ERROR 2002 (HY000): Can't connect to local MySQL server through socket",
            "ERROR 2013 (HY000): Lost connection to MySQL server at 'reading initial communication packet'",
            "ERROR 1040 (08004): Too many connections",
            "",
        ] {
            assert!(
                matches!(classify_client_error(starting), Failure::NotYet(_)),
                "{starting}"
            );
        }
    }

    #[test]
    fn only_a_mysql_packet_counts_as_the_servers_greeting() {
        assert!(is_mysql_greeting(&[0x4a, 0, 0, 0, 10, b'8']));
        assert!(is_mysql_greeting(&[0x1c, 0, 0, 0, 0xff, 0x10]));
        assert!(!is_mysql_greeting(&[0x4a, 0, 0, 0]));
        assert!(!is_mysql_greeting(&[0x4a, 0, 0, 0, 10]));
        assert!(!is_mysql_greeting(b"HTTP/1.1 400"));
        assert!(!is_mysql_greeting(b"+OK\r\n\r\n"));
    }

    #[test]
    fn the_bootstrap_initializes_only_when_asked_and_never_listens() {
        let init = bootstrap_script(true);
        let reuse = bootstrap_script(false);
        for script in [&init, &reuse] {
            assert!(script.starts_with("set -eu\n"));
            assert!(
                script.contains("exec gosu mysql mysqld --skip-networking --skip-grant-tables")
            );
            assert!(script.contains("chown mysql:mysql"));
        }
        assert!(init.contains("--initialize-insecure"));
        assert!(!reuse.contains("--initialize"), "a clone's data must exist");
        assert!(reuse.contains("exit 78"));
        // An empty directory is initialized; a directory with something else
        // in it is not touched.
        assert!(init.contains("ls -A") && init.contains("is not empty"));
    }

    #[test]
    fn the_copy_script_reflinks_always_and_drops_only_the_sources_identity() {
        assert!(COPY_SCRIPT.contains("--reflink=always"));
        assert!(!COPY_SCRIPT.contains("--reflink=auto"));
        assert!(
            COPY_SCRIPT.contains("ALLOW_COPY"),
            "a copy is the journaled decision"
        );
        assert!(
            COPY_SCRIPT.contains("cp -a "),
            "ownership and modes are kept"
        );
        // The identity files go only when the destination is a branch.
        let fixup = COPY_SCRIPT.split("FIXUP").nth(1).unwrap();
        for file in [
            "auto.cnf",
            "ca.pem",
            "server-cert.pem",
            "server-key.pem",
            "client-cert.pem",
            "private_key.pem",
            "public_key.pem",
        ] {
            assert!(fixup.contains(file), "{file}");
        }
        for kept in ["ibdata1", "mysql.ibd", "undo_", "#innodb_redo"] {
            assert!(!fixup.contains(kept), "{kept} is data, not identity");
        }
        // A directory that is not a data directory is not a source.
        assert!(COPY_SCRIPT.contains("result=empty"));
    }

    #[test]
    fn passwords_travel_on_standard_input_never_in_arguments() {
        // The client script reads the password from its first input line and
        // gives it to `mysql` in its own environment only.
        assert!(AUTH_SCRIPT.starts_with("IFS= read -r PW"));
        assert!(AUTH_SCRIPT.contains("MYSQL_PWD=\"$PW\" exec mysql \"$@\""));
        assert!(!AUTH_SCRIPT.contains("-p"));
        let login = Login {
            user: "app".into(),
            password: "pw".into(), // kingfisher:ignore
            database: "shop".into(),
        };
        let mut args = vec![
            "bash",
            "-c",
            AUTH_SCRIPT,
            "devflow-mysql",
            "-u",
            &login.user,
        ];
        args.push(&login.database);
        assert!(!args.iter().any(|a| a.contains(&login.password)));
    }
}
