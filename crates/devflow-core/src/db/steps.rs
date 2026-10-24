//! Single DDL steps of the operation table. Each one re-checks the project
//! lock first and runs on the session that holds it (`Session::admin`), so a
//! lost lock aborts the statement in flight. Statements that create or drop
//! databases run alone: PostgreSQL refuses them inside the implicit transaction
//! of a multi-statement batch.
use super::CloneStrategy;
use super::admin::{Session, has_code, password_verifier, quote_ident as q, quote_literal};
use super::meta;
use crate::config::{BusySourcePolicy, CowCapability};
use anyhow::{Context, Result, bail};
use postgres::error::SqlState;
use std::time::{Duration, Instant};

/// Attempts at `CREATE DATABASE … TEMPLATE` while terminating a busy source.
const BUSY_ATTEMPTS: u32 = 5;

/// How often a `wait` policy looks at the source's sessions.
const BUSY_POLL: Duration = Duration::from_millis(200);

/// Sessions a message spells out; the rest are counted.
const LISTED_SESSIONS: usize = 8;

/// A client session on a database, as much as a message may say about it:
/// who it says it is, where it connects from and what it is doing. Never the
/// query it runs.
#[derive(Debug)]
pub(super) struct ClientSession {
    pub pid: i32,
    /// Empty when the admin may not see it.
    pub user: String,
    pub application: String,
    /// `None` for a connection over the Unix socket.
    pub address: Option<String>,
    pub state: Option<String>,
}

impl ClientSession {
    /// A session as the server reports it, made safe to print. The application
    /// name is whatever the client set, so a credential can be in it, and
    /// PostgreSQL itself keeps only its first 63 bytes, so the `@` or the
    /// `password=` that identify one may already be gone. Every field is
    /// masked whole and conservatively first ([`crate::redact::text_cut`]: the
    /// `secrets` this session knows, the usual patterns, and everything after a
    /// `scheme://user:` or a `password=`), and only then are control characters
    /// dropped and the name cut short for the message.
    pub(super) fn new(
        (pid, user, application): (i32, &str, &str),
        address: Option<String>,
        state: Option<String>,
        secrets: &[&str],
    ) -> ClientSession {
        let safe = |text: &str| crate::redact::text_cut(text, secrets);
        let application = safe(application);
        // Dropping control characters can join the two halves of a secret.
        let application: String = safe(&application)
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        let application = safe(&application).chars().take(40).collect();
        ClientSession {
            pid,
            user: safe(user),
            application,
            address,
            state,
        }
    }

    /// `psql (app, pid 812, 172.17.0.1, idle in transaction)`.
    pub(super) fn describe(&self) -> String {
        let application = &self.application;
        let application = if application.trim().is_empty() {
            "unnamed client"
        } else {
            application.trim()
        };
        let user = if self.user.is_empty() {
            String::new()
        } else {
            format!("{}, ", self.user)
        };
        format!(
            "{application} ({user}pid {}, {}, {})",
            self.pid,
            self.address.as_deref().unwrap_or("local socket"),
            self.state.as_deref().unwrap_or("state unknown")
        )
    }
}

/// `a, b, c and 2 more`.
fn list_described(described: &[String]) -> String {
    let shown = described
        .iter()
        .take(LISTED_SESSIONS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match described.len().saturating_sub(LISTED_SESSIONS) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

fn list_sessions(sessions: &[ClientSession]) -> String {
    list_described(
        &sessions
            .iter()
            .map(ClientSession::describe)
            .collect::<Vec<_>>(),
    )
}

/// `: a, b` for a message that may or may not have sessions to name.
fn named(sessions: &[ClientSession]) -> String {
    if sessions.is_empty() {
        String::new()
    } else {
        format!(": {}", list_sessions(sessions))
    }
}

/// How long to pause before attempt `attempt + 1` at a busy source: an
/// exponential backoff, which under `wait` never reaches past `deadline` (the
/// attempt that follows is then admitted, or refused, against the deadline by
/// `Session::wait_until_idle`).
fn retry_pause(
    attempt: u32,
    policy: BusySourcePolicy,
    deadline: Instant,
    now: Instant,
) -> Duration {
    let backoff = Duration::from_millis(100 << (attempt - 1).min(4));
    match policy {
        BusySourcePolicy::Wait => backoff.min(deadline.saturating_duration_since(now)),
        _ => backoff,
    }
}

/// Attributes no devflow role ever has.
const PLAIN_ROLE: &str = "NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS";

impl Session<'_> {
    // ---- roles ----

    /// Let the admin act for `role` (a no-op for superusers and for roles the
    /// admin created in a session with `createrole_self_grant`).
    fn ensure_privileges_of(&mut self, role: &str) -> Result<()> {
        let ok: bool = self
            .admin
            .query_one(
                "SELECT pg_has_role(current_user, $1::name, 'USAGE') \
                    AND pg_has_role(current_user, $1::name, 'SET')",
                &[&role],
            )?
            .get(0);
        if !ok {
            self.admin
                .batch_execute(&format!(
                    "GRANT {} TO CURRENT_USER WITH INHERIT TRUE, SET TRUE",
                    q(role)
                ))
                .with_context(|| format!("the admin role cannot act as {role}"))?;
        }
        Ok(())
    }

    /// The project's NOLOGIN owner role: no attributes, no admin option.
    pub(super) fn ensure_owner_role(&mut self, owner: &str) -> Result<()> {
        self.step()?;
        if !self.role_exists(owner)? {
            self.admin
                .batch_execute(&format!("CREATE ROLE {} NOLOGIN {PLAIN_ROLE}", q(owner)))
                .context(
                    "cannot create the project owner role (the admin role needs CREATEROLE)",
                )?;
        }
        self.ensure_privileges_of(owner)
    }

    /// `LOGIN` member of the owner acting as it (`SET role`), with a SCRAM
    /// verifier instead of the clear password.
    ///
    /// The membership is `SET` only, never `INHERIT`. The session acts as the
    /// owner, so the owner is the role that needs `CONNECT` on the branch's
    /// databases (`pg_database_size(current_database())` and the GUI tools that
    /// call it check it); but the owner is shared by every branch of the
    /// project, so a login that inherited the owner would inherit `CONNECT` on
    /// its siblings' databases too. Without `INHERIT` the connection check
    /// (made for the login before `SET role` applies) sees only the login's own
    /// grant: one database.
    pub(super) fn create_login(&mut self, login: &str, owner: &str, password: &str) -> Result<()> {
        self.step()?;
        if self.role_exists(login)? {
            bail!("role {login} already exists");
        }
        self.admin
            .batch_execute(&format!(
                "CREATE ROLE {} LOGIN NOINHERIT {PLAIN_ROLE} PASSWORD {} IN ROLE {}",
                q(login),
                quote_literal(&password_verifier(password)),
                q(owner)
            ))
            .context("cannot create the branch login (the admin role needs CREATEROLE)")?;
        self.admin.batch_execute(&format!(
            "ALTER ROLE {} SET role = {}",
            q(login),
            quote_literal(owner)
        ))?;
        // Membership must allow SET ROLE, must not inherit, and never carries
        // ADMIN OPTION.
        let usable: Option<bool> = self
            .admin
            .query_one(
                "SELECT bool_or(NOT inherit_option AND set_option AND NOT admin_option) \
                 FROM pg_auth_members WHERE roleid = to_regrole($1) AND member = to_regrole($2)",
                &[&owner, &login],
            )?
            .get(0);
        if usable != Some(true) {
            self.admin.batch_execute(&format!(
                "GRANT {} TO {} WITH INHERIT FALSE, SET TRUE, ADMIN FALSE",
                q(owner),
                q(login)
            ))?;
        }
        Ok(())
    }

    /// Stop `login` inheriting the owner's privileges (a login made by an
    /// earlier build did), keeping `SET ROLE`.
    pub(super) fn stop_inheriting(&mut self, login: &str, owner: &str) -> Result<()> {
        self.step()?;
        self.admin
            .batch_execute(&format!(
                "GRANT {} TO {} WITH INHERIT FALSE, SET TRUE, ADMIN FALSE",
                q(owner),
                q(login)
            ))
            .with_context(|| format!("cannot stop {login} inheriting {owner}"))
    }

    pub(super) fn set_password(&mut self, login: &str, password: &str) -> Result<()> {
        self.step()?;
        self.admin
            .batch_execute(&format!(
                "ALTER ROLE {} PASSWORD {}",
                q(login),
                quote_literal(&password_verifier(password))
            ))
            .with_context(|| format!("cannot set the password of {login}"))
    }

    /// `DROP ROLE`; when other databases still reference the role (ACL
    /// entries, policies), disable it and record a tombstone instead.
    /// Returns whether the role is gone.
    pub(super) fn drop_login(&mut self, login: &str, reason: &str) -> Result<bool> {
        self.step()?;
        if !self.role_exists(login)? {
            return Ok(true);
        }
        match self.admin.batch_execute(&format!("DROP ROLE {}", q(login))) {
            Ok(()) => Ok(true),
            Err(e) if e.code() == Some(&SqlState::DEPENDENT_OBJECTS_STILL_EXIST) => {
                self.admin
                    .batch_execute(&format!("ALTER ROLE {} NOLOGIN PASSWORD NULL", q(login)))?;
                let project = self.project_id();
                meta::retire_role(&mut self.admin, project, login, reason)?;
                Ok(false)
            }
            Err(e) => Err(e).with_context(|| format!("cannot drop role {login}")),
        }
    }

    // ---- databases ----

    /// `CREATE DATABASE` closed to connections, from `template0` so a customized
    /// `template1` never leaks into branches.
    pub(super) fn create_empty_database(&mut self, db: &str) -> Result<()> {
        self.step()?;
        self.admin
            .batch_execute(&format!(
                "CREATE DATABASE {} TEMPLATE template0 ALLOW_CONNECTIONS false",
                q(db)
            ))
            .with_context(|| format!("cannot create database {db}"))
    }

    /// Clone `template` into the closed database `db` with `strategy`,
    /// applying the busy-source policy: fail, terminate the sessions and retry
    /// a bounded number of times, or wait for them to end.
    pub(super) fn clone_database(
        &mut self,
        template: &str,
        db: &str,
        strategy: CloneStrategy,
    ) -> Result<()> {
        let policy = self.server.settings.busy_policy();
        let wait = self.server.settings.busy_wait();
        let deadline = Instant::now() + wait;
        let attempts = match policy {
            BusySourcePolicy::Disconnect => BUSY_ATTEMPTS,
            BusySourcePolicy::Fail => 1,
            // The deadline bounds a wait, not a count.
            BusySourcePolicy::Wait => u32::MAX,
        };
        let sql = format!(
            "CREATE DATABASE {} TEMPLATE {} STRATEGY {} ALLOW_CONNECTIONS false",
            q(db),
            q(template),
            strategy.keyword()
        );
        let mut ended = Vec::new();
        let mut stuck = Vec::new();
        for attempt in 1..=attempts {
            self.step()?;
            match policy {
                BusySourcePolicy::Fail => {
                    let sessions = self.sessions_on(template)?;
                    if !sessions.is_empty() {
                        bail!(
                            "the source database has {} open session(s): {}; close them, or set \
                             on_busy_source to \"disconnect\" to end them or to \"wait\" to wait \
                             for them to end",
                            sessions.len(),
                            list_sessions(&sessions)
                        );
                    }
                }
                BusySourcePolicy::Disconnect => {
                    let (done, left) = self.terminate_sessions(template)?;
                    ended.extend(done);
                    stuck = left;
                }
                BusySourcePolicy::Wait => self.wait_until_idle(template, deadline, wait)?,
            }
            match self.admin.batch_execute(&sql) {
                Ok(()) => {
                    if !ended.is_empty() {
                        self.warnings.push(format!(
                            "terminated {} session(s) on the source database ({}); their open \
                             transactions were aborted",
                            ended.len(),
                            list_described(&ended)
                        ));
                    }
                    return Ok(());
                }
                Err(e) if e.code() == Some(&SqlState::OBJECT_IN_USE) => {
                    match policy {
                        BusySourcePolicy::Wait if Instant::now() < deadline => {}
                        BusySourcePolicy::Wait => {
                            let sessions = self.sessions_on(template)?;
                            bail!(
                                "the source database is still in use after waiting {}{}",
                                humantime::format_duration(wait),
                                named(&sessions)
                            );
                        }
                        _ if attempt < attempts => {}
                        BusySourcePolicy::Fail => bail!("the source database is in use"),
                        BusySourcePolicy::Disconnect => {
                            let hint = if stuck.is_empty() {
                                String::new()
                            } else {
                                format!(" (could not terminate: {})", stuck.join(", "))
                            };
                            bail!(
                                "the source database is still in use after {attempts} attempts{hint}"
                            )
                        }
                    }
                    std::thread::sleep(retry_pause(attempt, policy, deadline, Instant::now()));
                }
                Err(e) => return Err(e).context("cannot clone the source database"),
            }
        }
        bail!("the source database is in use")
    }

    /// Wait until `db` has no client sessions, and admit the clone only while
    /// `deadline` has not passed: once it has, fail naming the sessions that
    /// are left, even if they just closed (a retry after `object in use` never
    /// starts another clone past the budget).
    fn wait_until_idle(&mut self, db: &str, deadline: Instant, wait: Duration) -> Result<()> {
        loop {
            self.step()?;
            let sessions = self.sessions_on(db)?;
            let now = Instant::now();
            if now >= deadline {
                if sessions.is_empty() {
                    bail!(
                        "the source database was still in use when the {} wait ended",
                        humantime::format_duration(wait)
                    );
                }
                bail!(
                    "the source database still has {} open session(s) after waiting {}: {}",
                    sessions.len(),
                    humantime::format_duration(wait),
                    list_sessions(&sessions)
                );
            }
            if sessions.is_empty() {
                return Ok(());
            }
            std::thread::sleep(BUSY_POLL.min(deadline - now));
        }
    }

    /// The size of database `db` in bytes, or `None` when it cannot be read.
    pub(super) fn database_size(&mut self, db: &str) -> Option<u64> {
        let size: i64 = self
            .admin
            .query_one("SELECT pg_database_size($1::name)", &[&db])
            .ok()?
            .get(0);
        u64::try_from(size).ok()
    }

    /// Client sessions on `db` other than ours. Sessions of roles the admin
    /// has no privileges of (a superuser, say) show a NULL `backend_type`;
    /// they still block a clone, so they are listed too. Queries are never
    /// read.
    pub(super) fn sessions_on(&mut self, db: &str) -> Result<Vec<ClientSession>> {
        let secrets = self.target.secrets();
        let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
        Ok(self
            .admin
            .query(
                "SELECT pid, coalesce(usename, '')::text, coalesce(application_name, '')::text, \
                        client_addr::text, state::text \
                 FROM pg_stat_activity \
                 WHERE datname = $1 AND pid <> pg_backend_pid() \
                   AND (backend_type IS NULL OR backend_type = 'client backend')",
                &[&db],
            )?
            .iter()
            .map(|row| {
                ClientSession::new(
                    (row.get(0), row.get::<_, &str>(1), row.get::<_, &str>(2)),
                    row.get(3),
                    row.get(4),
                    &secrets,
                )
            })
            .collect())
    }

    /// Terminate other sessions on `db`. Returns the sessions that were
    /// terminated and the ones the admin is not allowed to signal, each
    /// described.
    pub(super) fn terminate_sessions(&mut self, db: &str) -> Result<(Vec<String>, Vec<String>)> {
        let mut done = Vec::new();
        let mut stuck = Vec::new();
        for session in self.sessions_on(db)? {
            match self
                .admin
                .query_one("SELECT pg_terminate_backend($1)", &[&session.pid])
            {
                Ok(row) if row.get::<_, bool>(0) => done.push(session.describe()),
                _ => stuck.push(session.describe()),
            }
        }
        Ok((done, stuck))
    }

    /// `PUBLIC` loses `CONNECT, TEMPORARY`; the project owner gets
    /// `CREATE, TEMPORARY` (no grant option). `CONNECT` comes last, from
    /// [`Session::grant_connect`].
    pub(super) fn restrict_database(&mut self, db: &str, owner: &str) -> Result<()> {
        self.step()?;
        self.admin
            .batch_execute(&format!(
                "REVOKE CONNECT, TEMPORARY ON DATABASE {db} FROM PUBLIC; \
                 GRANT CREATE, TEMPORARY ON DATABASE {db} TO {owner}",
                db = q(db),
                owner = q(owner)
            ))
            .with_context(|| format!("cannot set privileges on {db}"))
    }

    pub(super) fn set_connections(&mut self, db: &str, allow: bool) -> Result<()> {
        self.step()?;
        self.admin
            .batch_execute(&format!(
                "ALTER DATABASE {} ALLOW_CONNECTIONS {allow}",
                q(db)
            ))
            .with_context(|| format!("cannot change connections to {db}"))
    }

    /// `CONNECT` on `db` for the branch's login and for the project owner.
    /// The owner is the role the login's session acts as (`SET role`), so it
    /// is the one `has_database_privilege(current_database(), 'CONNECT')` and
    /// `pg_database_size` check. It is shared by the project's branches, which
    /// is safe only because no login inherits it ([`Session::create_login`]).
    pub(super) fn grant_connect(&mut self, db: &str, login: &str, owner: &str) -> Result<()> {
        self.step()?;
        self.admin
            .batch_execute(&format!(
                "GRANT CONNECT ON DATABASE {} TO {}, {}",
                q(db),
                q(login),
                q(owner)
            ))
            .with_context(|| format!("cannot grant {login} access to {db}"))
    }

    /// `CONNECT` on `db` for the owner alone (the login already has its own).
    pub(super) fn grant_owner_connect(&mut self, db: &str, owner: &str) -> Result<()> {
        self.step()?;
        self.admin
            .batch_execute(&format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                q(db),
                q(owner)
            ))
            .with_context(|| format!("cannot grant {owner} access to {db}"))
    }

    /// Make the project owner own everything in a new database.
    /// Empty database: the `public` schema. Clone of a branch: reassign the
    /// source login's objects, then refuse leftovers `REASSIGN OWNED` cannot
    /// move. Runs on a short connection to the new database (the only work not
    /// on the lock session): `reconcile` drops a half-made database WITH
    /// (FORCE), which ends that connection.
    pub(super) fn normalize(
        &mut self,
        db: &str,
        owner: &str,
        source_login: Option<&str>,
    ) -> Result<()> {
        self.step()?;
        if let Some(login) = source_login {
            self.ensure_privileges_of(login)?;
        }
        let sql = match source_login {
            None => format!("ALTER SCHEMA public OWNER TO {}", q(owner)),
            Some(login) => format!("REASSIGN OWNED BY {} TO {}", q(login), q(owner)),
        };
        let mut connection = self.target.connect(db)?;
        connection
            .batch_execute(&sql)
            .with_context(|| format!("cannot normalize ownership in {db}"))?;
        connection.close()?;
        if let Some(login) = source_login {
            self.refuse_login_references(db, login)?;
        }
        Ok(())
    }

    /// `REASSIGN OWNED` does not move default privileges, ACL entries naming the
    /// login, or policies naming it. `pg_shdepend` lists exactly those.
    /// Translating them is out of scope, so the clone fails.
    fn refuse_login_references(&mut self, db: &str, login: &str) -> Result<()> {
        let left: i64 = self
            .admin
            .query_one(
                "SELECT count(*) FROM pg_shdepend \
                 WHERE refclassid = 'pg_authid'::regclass AND refobjid = to_regrole($1)::oid \
                   AND dbid = (SELECT oid FROM pg_database WHERE datname = $2)",
                &[&login, &db],
            )?
            .get(0);
        if left > 0 {
            bail!(
                "source database has login-specific privileges/policies for {login}; \
                 use the project owner role for portable privileges"
            );
        }
        Ok(())
    }

    /// Whether `db` and everything in it live in the default tablespace. The
    /// CoW verification covers `$PGDATA`'s filesystem only.
    pub(super) fn on_default_tablespace(&mut self, db: &str) -> bool {
        self.admin
            .query_one(
                "SELECT d.dattablespace = (SELECT oid FROM pg_tablespace WHERE spcname = 'pg_default') \
                    AND NOT EXISTS (SELECT 1 FROM pg_tablespace t, \
                                    LATERAL pg_tablespace_databases(t.oid) o(oid) \
                                    WHERE t.spcname NOT IN ('pg_default', 'pg_global') AND o.oid = d.oid) \
                 FROM pg_database d WHERE d.datname = $1",
                &[&db],
            )
            .map(|row| row.get(0))
            .unwrap_or(false)
    }

    /// `DROP DATABASE … WITH (FORCE)`. Prepared transactions and replication
    /// slots would block it; they are reported, never forced away.
    pub(super) fn drop_database(&mut self, db: &str) -> Result<()> {
        self.step()?;
        if !self.database_exists(db)? {
            return Ok(());
        }
        let prepared: Vec<String> = self
            .admin
            .query(
                "SELECT gid FROM pg_prepared_xacts WHERE database = $1",
                &[&db],
            )
            .map(|rows| rows.iter().map(|r| r.get(0)).collect())
            .unwrap_or_default();
        let slots: Vec<String> = self
            .admin
            .query(
                "SELECT slot_name::text FROM pg_replication_slots WHERE database = $1",
                &[&db],
            )
            .map(|rows| rows.iter().map(|r| r.get(0)).collect())
            .unwrap_or_default();
        if !prepared.is_empty() || !slots.is_empty() {
            bail!(
                "database {db} cannot be dropped: prepared transactions [{}] and replication slots [{}] \
                 must be resolved first (ROLLBACK PREPARED / pg_drop_replication_slot)",
                prepared.join(", "),
                slots.join(", ")
            );
        }
        self.admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", q(db)))
            .with_context(|| format!("cannot drop database {db}"))
    }

    /// `ALTER DATABASE … RENAME` pair plus a metadata update, atomically, in one
    /// transaction on the lock session (connected to the metadata database, so
    /// the rows and the renames commit or die together with the lock). The
    /// caller closed both databases and terminated their sessions; a session
    /// that slipped in makes the transaction fail and the caller retries. The
    /// branch's `cow` is replaced by the capability of the new data, and its
    /// clone strategy by the one the reset journaled. The journal must still be
    /// this reset's (`cutover` at `generation`); returns the
    /// branch's new generation.
    pub(super) fn swap_databases(
        &mut self,
        branch: uuid::Uuid,
        generation: i32,
        original: &str,
        replacement: &str,
        backup: &str,
        cow: CowCapability,
    ) -> Result<i32> {
        self.step()?;
        let mut tx = self.admin.transaction()?;
        tx.batch_execute(&format!(
            "ALTER DATABASE {} RENAME TO {}; ALTER DATABASE {} RENAME TO {}",
            q(original),
            q(backup),
            q(replacement),
            q(original)
        ))?;
        let new_generation = tx
            .query_opt(
                "UPDATE branches SET op_phase = 'cleanup', generation = generation + 1, cow = $3, \
                        clone_strategy = op_clone_strategy, updated_at = now() \
                 WHERE id = $1 AND op = 'reset' AND op_phase = 'cutover' AND generation = $2 \
                 RETURNING generation",
                &[&branch, &generation, &meta::cow_str(cow)],
            )?
            // Dropping `tx` on a mismatch rolls the renames back.
            .ok_or_else(|| meta::lost_race("the reset record"))?
            .get::<_, i32>(0);
        tx.commit()?;
        Ok(new_generation)
    }
}

/// True when a failure means "database is in use".
pub(super) fn in_use(error: &anyhow::Error) -> bool {
    has_code(error, &SqlState::OBJECT_IN_USE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "Zx8Qm2LpV7nRt4YcB9sHd6WkJ3fAe5Gu"; // kingfisher:ignore

    fn described(application: &str, secrets: &[&str]) -> String {
        ClientSession::new(
            (812, "dfl_a1b2c3d4e5f6", application),
            Some("172.17.0.1".into()),
            Some("idle in transaction".into()),
            secrets,
        )
        .describe()
    }

    /// What PostgreSQL stores: at most 63 bytes of the name.
    fn server_side(name: &str, bytes: usize) -> String {
        name[..bytes.min(name.len())].to_string()
    }

    fn assert_hidden(text: &str, context: &str) {
        assert!(!text.contains(&PASSWORD[..4]), "{context}: {text}");
    }

    #[test]
    fn a_url_in_an_application_name_is_masked_before_it_is_cut_short() {
        // 13 characters of URL, then the password: cut to 40 characters first,
        // the `@` is gone and the password is not recognised as one.
        let text = described(&format!("postgres://u:{PASSWORD}@h/d"), &[]);
        assert!(!text.contains(PASSWORD), "{text}");
        assert!(!text.contains(&PASSWORD[..8]), "{text}");
        assert!(text.contains("pid 812") && text.contains("idle in transaction"));
    }

    #[test]
    fn a_name_the_server_already_cut_hides_the_password_at_every_length() {
        let url = format!("worker postgres://dfl_0123456789ab:{PASSWORD}@host/db");
        let conninfo = format!("host=h user=dfl_0123456789ab password={PASSWORD} dbname=d");
        let pwd = format!("app PWD = {PASSWORD}");
        let bare = format!("postgresql://dfl_0123456789ab:{PASSWORD}");
        for name in [&url, &conninfo, &pwd, &bare] {
            // Every cut the server could have made, and the 63-byte one.
            for bytes in 0..=name.len() {
                let cut = server_side(name, bytes);
                assert_hidden(&described(&cut, &[]), &cut);
            }
            let cut = server_side(name, 63);
            assert_hidden(&described(&cut, &[]), &cut);
        }
        // The case from review: 63 bytes keep 28 password characters and no `@`.
        let cut = server_side(&url, 63);
        assert!(!cut.contains('@') && cut.contains(&PASSWORD[..20]));
        let text = described(&cut, &[]);
        assert!(
            text.starts_with("worker postgres://dfl_0123456789ab:***"),
            "{text}"
        );
    }

    #[test]
    fn what_comes_before_the_credential_stays() {
        let text = described("batch job host=db password=hunter22 extra", &[]);
        assert!(text.starts_with("batch job host=db password=***"), "{text}");
        assert!(
            !text.contains("hunter22") && !text.contains("extra ("),
            "{text}"
        );
        // kingfisher:ignore
        let text = described("postgres://u:pw@h/d jobs", &[]);
        assert!(
            !text.contains("pw@") && text.contains("@h/d jobs"),
            "{text}"
        );
        // Nothing to hide, nothing masked.
        assert!(described("psql", &[]).starts_with("psql ("));
        assert!(described("postgres://h/d", &[]).starts_with("postgres://h/d ("));
    }

    #[test]
    fn known_secrets_are_masked_wherever_the_cut_would_fall() {
        let secret = "hunter2hunter2hunter2"; // kingfisher:ignore
        for padding in [0, 10, 30, 39] {
            let text = described(&format!("{}{secret}", "a".repeat(padding)), &[secret]);
            assert!(!text.contains(&secret[..4]), "{padding}: {text}");
        }
        // A control character in the middle does not hide it.
        let text = described("hunter2\u{1}hunter2hunter2", &[secret]);
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn a_waiting_retry_never_sleeps_past_the_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(150);
        // Far from the deadline the backoff is the usual one; near it, what is left.
        assert_eq!(
            retry_pause(
                1,
                BusySourcePolicy::Wait,
                now + Duration::from_secs(60),
                now
            ),
            Duration::from_millis(100)
        );
        assert_eq!(
            retry_pause(5, BusySourcePolicy::Wait, deadline, now),
            Duration::from_millis(150)
        );
        // Past it there is nothing to sleep, and `disconnect` keeps its backoff.
        assert_eq!(
            retry_pause(
                5,
                BusySourcePolicy::Wait,
                deadline,
                deadline + Duration::from_secs(1)
            ),
            Duration::ZERO
        );
        assert_eq!(
            retry_pause(3, BusySourcePolicy::Disconnect, deadline, deadline),
            Duration::from_millis(400)
        );
    }

    #[test]
    fn names_are_cut_short_and_an_empty_one_is_unnamed() {
        let text = described(&"x".repeat(100), &[]);
        assert!(text.starts_with(&format!("{} (", "x".repeat(40))), "{text}");
        assert!(described("\u{7}\u{1b}", &[]).starts_with("unnamed client ("));
    }
}
