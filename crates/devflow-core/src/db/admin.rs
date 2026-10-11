//! Admin connections, generated names, SQL quoting, and the per-project
//! session advisory lock held for the duration of a mutation.
use super::Server;
use super::meta::{self, BOOTSTRAP_PREFIX, FOREIGN_META_DB, META_DB, META_MARKER};
use crate::config::PostgresMode;
use anyhow::{Context, Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use postgres::config::Host;
use postgres::error::SqlState;
use postgres::{Client, Config, NoTls};
use rand::distr::{Alphanumeric, SampleString};
use std::net::IpAddr;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a mutation waits for another devflow process that holds the same
/// project lock before giving up (a hung `pg_advisory_lock` would block forever).
const LOCK_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Serializes bootstrapping of the metadata database across processes
/// ("devflow" + 2). Taken on the `postgres` database, which exists before
/// `devflow` does.
const BOOTSTRAP_LOCK_KEY: i64 = 0x6465_7666_6c6f_7702;

// ---- names and quoting ----------------------------------------------------

/// Hex prefix of a uuid, lowercase, no dashes.
fn hex(id: Uuid, len: usize) -> String {
    id.simple().to_string()[..len].to_string()
}

/// `df_<project8>_<id12>`: branch, snapshot and staging databases.
pub(super) fn db_name(project: Uuid, id: Uuid) -> String {
    format!("df_{}_{}", hex(project, 8), hex(id, 12))
}

/// `df_<project8>_<branch12>_<seq>`: the `seq`-th scratch database of a
/// branch. Generated like every name (`seq` is allocated per branch and
/// recorded with the row); the configured label never reaches a name.
pub(super) fn scratch_db_name(project: Uuid, branch: Uuid, seq: i32) -> String {
    format!("{}_{seq}", db_name(project, branch))
}

/// `dfl_<id12>`: a branch's login role.
pub(super) fn login_name(id: Uuid) -> String {
    format!("dfl_{}", hex(id, 12))
}

/// `dfo_<project8>`: the project's NOLOGIN owner role.
pub(super) fn owner_name(project: Uuid) -> String {
    format!("dfo_{}", hex(project, 8))
}

/// Identifiers in DDL are always generated names, never user input; quoting is
/// defence in depth.
pub(super) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(super) fn quote_literal(value: &str) -> String {
    let escaped = value.replace('\'', "''");
    if escaped.contains('\\') {
        format!("E'{}'", escaped.replace('\\', "\\\\"))
    } else {
        format!("'{escaped}'")
    }
}

pub(super) fn random_password() -> String {
    Alphanumeric.sample_string(&mut rand::rng(), 32)
}

/// A SCRAM-SHA-256 verifier. Sending it instead of the clear text keeps
/// passwords out of server logs and `pg_stat_statements`.
pub(super) fn password_verifier(password: &str) -> String {
    postgres_protocol::password::scram_sha_256(password.as_bytes())
}

/// Everything except the RFC 3986 unreserved characters is percent-encoded.
const URL_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Application URL for a branch login.
pub(super) fn url(host: &str, port: u16, user: &str, password: &str, database: &str) -> String {
    let enc = |s: &str| utf8_percent_encode(s, URL_COMPONENT).to_string();
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!(
        "postgres://{}:{}@{host}:{port}/{}",
        enc(user),
        enc(password),
        enc(database)
    )
}

/// First 8 bytes of the project id as the advisory lock key.
pub(super) fn lock_key(project: Uuid) -> i64 {
    let b = project.as_bytes();
    i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Address a client uses to reach a published port bound to `bind`.
pub(super) fn connect_host(bind: &str) -> String {
    match bind.parse::<IpAddr>() {
        Ok(ip) if ip.is_unspecified() => if ip.is_ipv4() { "127.0.0.1" } else { "::1" }.to_string(),
        _ => bind.to_string(),
    }
}

// ---- errors ---------------------------------------------------------------

/// True when `error` or one of its causes is a database error with `code`.
pub(super) fn has_code(error: &anyhow::Error, code: &SqlState) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<postgres::Error>())
        .any(|e| e.code() == Some(code))
}

use crate::error::LockLost;

pub(super) fn lock_lost(error: &anyhow::Error) -> bool {
    error.downcast_ref::<LockLost>().is_some()
}

// ---- endpoint -------------------------------------------------------------

/// Where and as whom the admin connects.
pub(super) struct Target {
    config: Config,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) user: String,
}

impl Target {
    /// Managed: the published container port with the generated superuser
    /// password. External: `DEVFLOW_PG_ADMIN_URL`, else `admin_url`.
    pub(super) fn resolve(server: &Server) -> Result<Target> {
        Self::resolve_within(server, super::managed::QUERY_WITHIN)
    }

    /// [`Target::resolve`] with the lookup of a managed server's published
    /// port bounded by `budget` (the time a caller waiting on a deadline has
    /// left): Docker that stalls is killed, not waited for.
    pub(super) fn resolve_within(server: &Server, budget: Duration) -> Result<Target> {
        let settings = &server.settings;
        match (&server.managed, settings.mode) {
            (Some(managed), PostgresMode::Managed) => {
                let port = if settings.port != 0 {
                    settings.port
                } else {
                    managed.published_port_within(budget)?
                };
                let host = connect_host(&settings.bind);
                let mut config = Config::new();
                config
                    .host(&host)
                    .port(port)
                    .user("postgres")
                    .password(managed.admin_password()?);
                Ok(Target {
                    config,
                    host,
                    port,
                    user: "postgres".into(),
                })
            }
            _ => {
                let url = std::env::var("DEVFLOW_PG_ADMIN_URL")
                    .ok()
                    .filter(|u| !u.trim().is_empty())
                    .or_else(|| settings.admin_url.clone())
                    .context(
                        "external mode needs an admin URL: set DEVFLOW_PG_ADMIN_URL or postgres.admin_url",
                    )?;
                let config = parse_admin_url(&url)?;
                let host = match config.get_hosts().first() {
                    Some(Host::Tcp(host)) => host.clone(),
                    _ => bail!("the admin URL must name a TCP host"),
                };
                let port = config.get_ports().first().copied().unwrap_or(5432);
                let user = config
                    .get_user()
                    .context("the admin URL must name a user")?
                    .to_string();
                Ok(Target {
                    config,
                    host,
                    port,
                    user,
                })
            }
        }
    }

    /// A target for tests: a plain endpoint with a user and no password.
    #[cfg(test)]
    pub(super) fn for_endpoint(host: &str, port: u16, user: &str) -> Target {
        let mut config = Config::new();
        config.host(host).port(port).user(user);
        Target {
            config,
            host: host.to_string(),
            port,
            user: user.to_string(),
        }
    }

    pub(super) fn connect(&self, dbname: &str) -> Result<Client> {
        let mut config = self.config.clone();
        config
            .dbname(dbname)
            .application_name("devflow")
            .connect_timeout(CONNECT_TIMEOUT);
        config.connect(NoTls).with_context(|| {
            format!(
                "cannot connect to PostgreSQL at {}:{} as {} (database {dbname})",
                self.host, self.port, self.user
            )
        })
    }

    /// Whether something on this endpoint answers like a PostgreSQL server,
    /// within `timeout` for each step. A server replies to an `SSLRequest`
    /// with one byte (`S` or `N`) before any authentication, so this tells a
    /// server from a process that accepts TCP connections and never speaks
    /// (a stale proxy, a foreign listener), which a regular connection would
    /// wait on without end.
    pub(super) fn probe(&self, timeout: Duration) -> Result<()> {
        speaks_postgres(&self.host, self.port, timeout)
    }

    /// Host and port for workspace URLs.
    pub(super) fn app_endpoint(&self, settings: &crate::config::PostgresSection) -> (String, u16) {
        (
            settings
                .app_host
                .clone()
                .unwrap_or_else(|| self.host.clone()),
            settings.app_port.unwrap_or(self.port),
        )
    }
}

/// Read the admin URL. The parser's error names the option it did not
/// understand, which in a mistyped URL can be part of the password, so it is
/// not shown.
fn parse_admin_url(url: &str) -> Result<Config> {
    url.parse()
        .map_err(|_| anyhow::anyhow!("the admin URL is not a valid PostgreSQL connection string"))
}

/// [`Target::probe`] for `host:port`.
pub(super) fn speaks_postgres(host: &str, port: u16, timeout: Duration) -> Result<()> {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    // Protocol 1234.5679: the SSL negotiation request, eight bytes.
    const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 4, 0xd2, 0x16, 0x2f];
    let address = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {host}"))?
        .next()
        .with_context(|| format!("{host} resolves to no address"))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .with_context(|| format!("nothing accepts connections at {host}:{port}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(&SSL_REQUEST)?;
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer).with_context(|| {
        format!("{host}:{port} accepts connections but does not answer like PostgreSQL")
    })?;
    anyhow::ensure!(
        matches!(answer[0], b'S' | b'N'),
        "{host}:{port} answers, but not like PostgreSQL"
    );
    Ok(())
}

// ---- server facts ---------------------------------------------------------

pub(super) struct Facts {
    pub(super) version_num: i32,
    pub(super) version: String,
    /// `None` before PostgreSQL 18.
    pub(super) file_copy_method: Option<String>,
    pub(super) postmaster_start: String,
}

impl Facts {
    pub(super) fn read(client: &mut Client) -> Result<Facts> {
        let row = client
            .query_one(
                "SELECT current_setting('server_version_num')::int, \
                        current_setting('server_version'), \
                        current_setting('file_copy_method', true), \
                        pg_postmaster_start_time()::text",
                &[],
            )
            .context("cannot read server settings")?;
        Ok(Facts {
            version_num: row.get(0),
            version: row.get(1),
            file_copy_method: row.get(2),
            postmaster_start: row.get(3),
        })
    }

    /// devflow clones with `file_copy_method` (PostgreSQL 18).
    pub(super) fn check(&self) -> Result<()> {
        if self.version_num < 180_000 {
            bail!(
                "devflow needs PostgreSQL 18 or newer, but this server runs {}",
                self.version
            );
        }
        Ok(())
    }

    pub(super) fn clone_enabled(&self) -> bool {
        self.file_copy_method.as_deref() == Some("clone")
    }
}

// ---- session --------------------------------------------------------------

/// Everything a mutation needs: the `admin` connection, which holds the
/// project's advisory lock and runs every cluster-level statement and every
/// metadata write.
///
/// DDL (`CREATE`/`DROP DATABASE`, role changes, `ALTER DATABASE`, `GRANT` on
/// databases, `pg_terminate_backend`) and every write to the metadata rows (the
/// operation journal included) must run on `admin` and nowhere else: when that
/// session dies, PostgreSQL releases the lock and aborts the statement in
/// flight together, so an operation that lost its lock can never complete a
/// statement, or a journal write, behind the back of the `reconcile` that took
/// the lock over. (The lock is per database, so `admin` stays connected to the
/// metadata database; cluster DDL works from any database.) Work inside a new
/// database (`normalize`) uses a short extra connection; `reconcile` drops
/// half-made databases `WITH (FORCE)`, which ends such a connection.
pub(super) struct Session<'a> {
    pub(super) server: &'a Server,
    pub(super) target: Target,
    pub(super) admin: Client,
    pub(super) facts: Facts,
    pub(super) warnings: Vec<String>,
}

impl<'a> Session<'a> {
    /// Connect, verify the server, bootstrap the metadata database and take the
    /// project lock. Does not touch project rows (see `ops::begin`).
    pub(super) fn open(server: &'a Server) -> Result<Session<'a>> {
        let target = Target::resolve(server)?;
        let facts = {
            // Bootstrap is serialized by its own lock on the `postgres` database
            // and cannot use the project lock: that lives in the database it creates.
            let mut setup = target.connect("postgres")?;
            let facts = Facts::read(&mut setup)?;
            facts.check()?;
            bootstrap_databases(&mut setup, server.managed.is_some())?;
            facts
        };
        // A short connection: the session keeps no metadata connection besides
        // the lock holder.
        meta::migrate(&mut target.connect(META_DB)?)?;
        let mut admin = acquire_lock(&target, server.project_id)?;
        // Lets the admin act for the roles it creates (non-superuser provisioners).
        admin
            .batch_execute("SET createrole_self_grant = 'inherit,set'")
            .context("cannot configure the provisioning session")?;
        Ok(Session {
            server,
            target,
            admin,
            facts,
            warnings: Vec::new(),
        })
    }

    pub(super) fn project_id(&self) -> Uuid {
        self.server.project_id
    }

    /// Check the lock connection before an external step; an error means the
    /// lock is gone and the operation must stop, leaving its row for reconcile.
    pub(super) fn step(&mut self) -> Result<()> {
        self.admin
            .simple_query("SELECT 1")
            .map(|_| ())
            .map_err(|_| {
                anyhow::Error::new(LockLost {
                    root: self.server.root.clone(),
                })
            })
    }

    /// Classify a failed step. A terminated lock session aborts its statement
    /// in flight, which surfaces as an ordinary database error; if the lock is
    /// gone that is what happened, and the caller must stop without touching
    /// the row (another process may already be recovering it).
    pub(super) fn lock_failure(&mut self, error: anyhow::Error) -> anyhow::Error {
        if lock_lost(&error) {
            return error;
        }
        if error.downcast_ref::<super::meta::LostRace>().is_some() {
            return self.needs_reconcile(format!("{error:#}"));
        }
        match self.step() {
            Ok(()) => error,
            Err(lost) => lost,
        }
    }

    /// An error that says what happened and carries the remedy: finish the
    /// interrupted work.
    pub(super) fn needs_reconcile(&self, message: impl Into<String>) -> anyhow::Error {
        crate::remedy::Problem::new(message)
            .with(crate::remedy::Remedy::ReconcileDatabases {
                root: self.server.root.clone(),
            })
            .into_error()
    }

    pub(super) fn database_exists(&mut self, name: &str) -> Result<bool> {
        Ok(self
            .admin
            .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&name])?
            .is_some())
    }

    pub(super) fn role_exists(&mut self, name: &str) -> Result<bool> {
        Ok(self
            .admin
            .query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&name])?
            .is_some())
    }
}

/// Bring the `devflow` metadata database into place closed and marked, take
/// `PUBLIC` connections away, then open it, so branch logins can never connect
/// there (advisory locks are per database; connecting would let a sandbox stall
/// provisioning). In managed mode devflow owns the whole server, so `postgres`
/// and `template1` are closed to `PUBLIC` as well; an external server's other
/// databases are never altered. The whole bootstrap runs under an advisory lock
/// on the `postgres` database, so processes take turns; each step still only
/// runs when needed and is retried (`tuple concurrently updated`) after
/// re-checking. The lock dies with `admin`, whatever way the bootstrap ends.
fn bootstrap_databases(admin: &mut Client, managed: bool) -> Result<()> {
    lock_session(admin, BOOTSTRAP_LOCK_KEY, "bootstrapping this server")?;
    claim_meta_database(admin)?;
    let mut databases = vec![META_DB];
    if managed {
        databases.extend(["postgres", "template1"]);
    }
    for db in databases {
        settle(
            admin,
            |c| public_can_connect(c, db),
            |c| {
                c.batch_execute(&format!(
                    "REVOKE CONNECT, TEMPORARY ON DATABASE {} FROM PUBLIC",
                    quote_ident(db)
                ))
            },
        )
        .with_context(|| format!("cannot restrict database {db}"))?;
    }
    settle(
        admin,
        |c| {
            Ok(!c
                .query_one(
                    "SELECT datallowconn FROM pg_database WHERE datname = $1",
                    &[&META_DB],
                )?
                .get::<_, bool>(0))
        },
        |c| {
            c.batch_execute(&format!(
                "ALTER DATABASE {} ALLOW_CONNECTIONS true",
                quote_ident(META_DB)
            ))
        },
    )
    .context("cannot open the `devflow` metadata database")
}

/// `Some(marked)` when a database named `devflow` exists.
fn meta_database_marked(admin: &mut Client) -> Result<Option<bool>, postgres::Error> {
    Ok(admin
        .query_opt(
            "SELECT coalesce(shobj_description(oid, 'pg_database') = $2, false) \
             FROM pg_database WHERE datname = $1",
            &[&META_DB, &META_MARKER],
        )?
        .map(|row| row.get(0)))
}

/// Make sure a marked `devflow` database exists, or refuse the one that is
/// there. devflow never creates `devflow` directly: it builds a closed staging
/// database `devflow_bootstrap_<random>`, marks it, and renames it into place
/// (only when `devflow` does not exist). A crash at any point leaves either
/// nothing, an unmarked staging database (never adopted or dropped; reported by
/// [`bootstrap_leftovers`]; the retry stages a new one), or a marked staging
/// database that the retry renames into place. A `devflow` database without the
/// mark is therefore never ours and is refused untouched. Runs under the
/// bootstrap lock, so no other devflow process is between these steps.
fn claim_meta_database(admin: &mut Client) -> Result<()> {
    match meta_database_marked(admin)? {
        Some(true) => return Ok(()),
        Some(false) => bail!("{FOREIGN_META_DB}"),
        None => {}
    }
    let staging = match marked_staging(admin)? {
        Some(name) => name,
        None => stage_meta_database(admin)?,
    };
    match admin.batch_execute(&format!(
        "ALTER DATABASE {} RENAME TO {}",
        quote_ident(&staging),
        quote_ident(META_DB)
    )) {
        Ok(()) => Ok(()),
        // Something created `devflow` meanwhile (not under our lock): it only
        // counts if it carries the mark.
        Err(e) if e.code() == Some(&SqlState::DUPLICATE_DATABASE) => {
            match meta_database_marked(admin)? {
                Some(true) => Ok(()),
                _ => bail!("{FOREIGN_META_DB}"),
            }
        }
        Err(e) => Err(e)
            .with_context(|| format!("cannot move the staged database {staging} to `devflow`")),
    }
}

/// A staging database that carries the mark: devflow's own, left by a crash
/// between marking and renaming. The name only narrows the search; the mark is
/// what makes it ours.
fn marked_staging(admin: &mut Client) -> Result<Option<String>, postgres::Error> {
    Ok(admin
        .query_opt(
            "SELECT datname::text FROM pg_database \
             WHERE datname LIKE $1 AND shobj_description(oid, 'pg_database') = $2 \
             ORDER BY datname LIMIT 1",
            &[&staging_pattern(), &META_MARKER],
        )?
        .map(|row| row.get(0)))
}

/// `CREATE DATABASE` closed, then the mark. The mark is the last step before the
/// rename, so an unmarked staging database is never mistaken for ours.
fn stage_meta_database(admin: &mut Client) -> Result<String> {
    let name = staging_name(Uuid::new_v4());
    admin
        .batch_execute(&format!(
            "CREATE DATABASE {} TEMPLATE template0 ALLOW_CONNECTIONS false",
            quote_ident(&name)
        ))
        .context("cannot create the `devflow` metadata database (the admin role needs CREATEDB)")?;
    admin
        .batch_execute(&format!(
            "COMMENT ON DATABASE {} IS {}",
            quote_ident(&name),
            quote_literal(META_MARKER)
        ))
        .with_context(|| {
            format!(
                "cannot mark the staging database {name}; it is left in place, drop it manually"
            )
        })?;
    Ok(name)
}

/// `devflow_bootstrap_<id12>`.
fn staging_name(id: Uuid) -> String {
    format!("{BOOTSTRAP_PREFIX}{}", hex(id, 12))
}

/// `LIKE` pattern for staging databases (underscores are literal).
fn staging_pattern() -> String {
    format!("{}%", BOOTSTRAP_PREFIX.replace('_', "\\_"))
}

/// Staging databases still around once `devflow` is in place: an interrupted
/// bootstrap's. devflow never adopts or drops them (it does not infer ownership
/// from names), so the operator is told which to drop.
pub(super) fn bootstrap_leftovers(admin: &mut Client) -> Result<Vec<String>, postgres::Error> {
    Ok(admin
        .query(
            "SELECT datname::text, coalesce(shobj_description(oid, 'pg_database') = $2, false) \
             FROM pg_database WHERE datname LIKE $1 ORDER BY 1",
            &[&staging_pattern(), &META_MARKER],
        )?
        .iter()
        .map(|row| {
            let name: String = row.get(0);
            let state = if row.get(1) {
                "marked by devflow but unused"
            } else {
                "unmarked, left by an interrupted bootstrap or by something else"
            };
            format!(
                "database {name} is {state}; devflow never adopts or drops it, \
                 run DROP DATABASE \"{name}\" if it is not needed"
            )
        })
        .collect())
}

fn public_can_connect(admin: &mut Client, db: &str) -> Result<bool, postgres::Error> {
    Ok(admin
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_database d, \
                    aclexplode(coalesce(d.datacl, acldefault('d', d.datdba))) a \
                    WHERE d.datname = $1 AND a.grantee = 0 AND a.privilege_type = 'CONNECT')",
            &[&db],
        )?
        .get(0))
}

/// Run `fix` while `needed` says so, retrying a few times: a concurrent
/// bootstrap by another process can make a statement fail spuriously.
fn settle(
    admin: &mut Client,
    mut needed: impl FnMut(&mut Client) -> Result<bool, postgres::Error>,
    fix: impl Fn(&mut Client) -> Result<(), postgres::Error>,
) -> Result<()> {
    let mut attempt = 0;
    loop {
        if !needed(admin)? {
            return Ok(());
        }
        match fix(admin) {
            Ok(()) => return Ok(()),
            Err(e) if attempt >= 5 => return Err(e.into()),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// `pg_try_advisory_lock` in a bounded loop instead of a blocking
/// `pg_advisory_lock`, so a stuck peer produces an error instead of a hang.
/// Session level: it lasts as long as `client`. `what` completes the timeout error.
fn lock_session(client: &mut Client, key: i64, what: &str) -> Result<()> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        let got: bool = client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&key])?
            .get(0);
        if got {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "another devflow operation has been {what} for over {}s",
                LOCK_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// A connection to the metadata database that holds the project lock.
fn acquire_lock(target: &Target, project: Uuid) -> Result<Client> {
    let mut client = target.connect(META_DB)?;
    lock_session(
        &mut client,
        lock_key(project),
        "holding this project's lock",
    )?;
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    #[test]
    fn a_bad_admin_url_is_refused_without_its_text() {
        for url in [
            "postgres://u@h/db?REVIEWSECRET=1",
            "REVIEWSECRET=abc",
            "host=h port=REVIEWSECRET",
            "REVIEWSECRET",
        ] {
            let error = parse_admin_url(url).unwrap_err();
            let shown = format!("{error} | {error:#} | {error:?}");
            assert!(shown.contains("the admin URL is not a valid"), "{shown}");
            assert!(!shown.contains("REVIEWSECRET"), "{url}\n{shown}");
        }
        assert!(parse_admin_url("postgres://admin@db.internal/postgres").is_ok());
    }

    /// A listener on a free loopback port that runs `behave` for each
    /// connection on its own thread.
    fn listener(behave: fn(std::net::TcpStream)) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || behave(stream));
            }
        });
        port
    }

    #[test]
    fn a_listener_that_accepts_and_never_answers_fails_the_probe_within_its_timeout() {
        // Holds the connection open and says nothing, like a stale proxy.
        let silent = listener(|mut stream| {
            let mut sink = [0u8; 64];
            while stream.read(&mut sink).is_ok_and(|n| n > 0) {}
        });
        let started = Instant::now();
        let error = speaks_postgres("127.0.0.1", silent, Duration::from_millis(300)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            format!("{error:#}").contains("does not answer like PostgreSQL"),
            "{error:#}"
        );
    }

    #[test]
    fn the_probe_accepts_what_answers_the_ssl_request_and_refuses_the_rest() {
        for answer in [b'S', b'N'] {
            let port = listener(if answer == b'S' {
                |mut s| {
                    let mut request = [0u8; 8];
                    let _ = s.read_exact(&mut request);
                    let _ = s.write_all(b"S");
                }
            } else {
                |mut s| {
                    let mut request = [0u8; 8];
                    let _ = s.read_exact(&mut request);
                    let _ = s.write_all(b"N");
                }
            });
            speaks_postgres("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        }
        let http = listener(|mut s| {
            let mut request = [0u8; 8];
            let _ = s.read_exact(&mut request);
            let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        });
        let error = speaks_postgres("127.0.0.1", http, Duration::from_secs(2)).unwrap_err();
        assert!(
            error.to_string().contains("not like PostgreSQL"),
            "{error:#}"
        );
        // Nothing listening at all.
        let closed = {
            let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            probe.local_addr().unwrap().port()
        };
        assert!(speaks_postgres("127.0.0.1", closed, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn generated_names_follow_the_scheme() {
        let project: Uuid = "0b3c9d2e-1111-4222-8333-444455556666".parse().unwrap();
        let id: Uuid = "A1B2C3D4-E5F6-4789-8ABC-DEF012345678".parse().unwrap();
        assert_eq!(db_name(project, id), "df_0b3c9d2e_a1b2c3d4e5f6");
        assert_eq!(login_name(id), "dfl_a1b2c3d4e5f6");
        assert_eq!(owner_name(project), "dfo_0b3c9d2e");
        assert_eq!(
            scratch_db_name(project, id, 2),
            "df_0b3c9d2e_a1b2c3d4e5f6_2"
        );
        for name in [
            db_name(project, id),
            login_name(id),
            owner_name(project),
            scratch_db_name(project, id, 12),
        ] {
            assert!(name.len() < 63);
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            );
        }
    }

    #[test]
    fn staging_databases_are_named_and_matched_by_a_literal_prefix() {
        let id: Uuid = "A1B2C3D4-E5F6-4789-8ABC-DEF012345678".parse().unwrap();
        assert_eq!(staging_name(id), "devflow_bootstrap_a1b2c3d4e5f6");
        assert!(staging_name(id).len() < 63);
        // `_` is a LIKE wildcard: it must be escaped to match literally.
        assert_eq!(staging_pattern(), "devflow\\_bootstrap\\_%");
    }

    #[test]
    fn quoting_escapes_embedded_quotes() {
        assert_eq!(quote_ident("df_abc"), "\"df_abc\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_literal("o'brien"), "'o''brien'");
        assert_eq!(quote_literal("a\\b"), "E'a\\\\b'");
        assert_eq!(quote_literal("plain"), "'plain'");
    }

    #[test]
    fn passwords_are_32_alphanumeric_characters() {
        let a = random_password();
        let b = random_password();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
        assert!(password_verifier(&a).starts_with("SCRAM-SHA-256$4096:"));
    }

    #[test]
    fn urls_encode_credentials_and_bracket_ipv6() {
        assert_eq!(
            url("db.internal", 5432, "dfl_a", "p@ss/w", "df_x"),
            "postgres://dfl_a:p%40ss%2Fw@db.internal:5432/df_x" // kingfisher:ignore
        );
        assert_eq!(url("::1", 1, "u", "p", "d"), "postgres://u:p@[::1]:1/d");
    }

    #[test]
    fn lock_key_uses_the_first_eight_bytes() {
        let id: Uuid = "00000000-0000-0001-ffff-ffffffffffff".parse().unwrap();
        assert_eq!(lock_key(id), 1);
        let id: Uuid = "ffffffff-ffff-ffff-0000-000000000000".parse().unwrap();
        assert_eq!(lock_key(id), -1);
    }

    #[test]
    fn unspecified_bind_addresses_connect_through_loopback() {
        assert_eq!(connect_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(connect_host("::"), "::1");
        assert_eq!(connect_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(connect_host("100.64.0.2"), "100.64.0.2");
    }

    #[test]
    fn old_servers_are_refused() {
        let facts = |n: i32, v: &str| Facts {
            version_num: n,
            version: v.into(),
            file_copy_method: None,
            postmaster_start: String::new(),
        };
        let err = facts(170_005, "17.5").check().unwrap_err().to_string();
        assert!(
            err.contains("PostgreSQL 18") && err.contains("17.5"),
            "{err}"
        );
        assert!(facts(180_006, "18.6").check().is_ok());
    }
}
