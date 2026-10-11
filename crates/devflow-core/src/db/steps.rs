//! Single DDL steps of the operation table. Each one re-checks the project
//! lock first and runs on the session that holds it (`Session::admin`), so a
//! lost lock aborts the statement in flight. Statements that create or drop
//! databases run alone: PostgreSQL refuses them inside the implicit transaction
//! of a multi-statement batch.
use super::admin::{Session, has_code, password_verifier, quote_ident as q, quote_literal};
use super::meta;
use crate::config::{BusySourcePolicy, CowCapability};
use anyhow::{Context, Result, bail};
use postgres::error::SqlState;
use std::time::Duration;

/// Attempts at `CREATE DATABASE … TEMPLATE` while terminating a busy source.
const BUSY_ATTEMPTS: u32 = 5;

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
    pub(super) fn create_login(&mut self, login: &str, owner: &str, password: &str) -> Result<()> {
        self.step()?;
        if self.role_exists(login)? {
            bail!("role {login} already exists");
        }
        self.admin
            .batch_execute(&format!(
                "CREATE ROLE {} LOGIN INHERIT {PLAIN_ROLE} PASSWORD {} IN ROLE {}",
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
        // Membership must inherit and allow SET ROLE, and never carry ADMIN OPTION.
        let usable: Option<bool> = self
            .admin
            .query_one(
                "SELECT bool_or(inherit_option AND set_option AND NOT admin_option) \
                 FROM pg_auth_members WHERE roleid = to_regrole($1) AND member = to_regrole($2)",
                &[&owner, &login],
            )?
            .get(0);
        if usable != Some(true) {
            self.admin.batch_execute(&format!(
                "GRANT {} TO {} WITH INHERIT TRUE, SET TRUE, ADMIN FALSE",
                q(owner),
                q(login)
            ))?;
        }
        Ok(())
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

    /// Clone `template` into the closed database `db`, applying the busy-source
    /// policy: fail, or terminate the sessions and retry a bounded number of times.
    pub(super) fn clone_database(&mut self, template: &str, db: &str) -> Result<()> {
        let policy = self.server.settings.busy_policy();
        let attempts = match policy {
            BusySourcePolicy::Disconnect => BUSY_ATTEMPTS,
            BusySourcePolicy::Fail => 1,
        };
        let sql = format!(
            "CREATE DATABASE {} TEMPLATE {} STRATEGY FILE_COPY ALLOW_CONNECTIONS false",
            q(db),
            q(template)
        );
        let mut terminated = 0;
        let mut stuck = Vec::new();
        for attempt in 1..=attempts {
            self.step()?;
            match policy {
                BusySourcePolicy::Fail => {
                    let sessions = self.sessions_on(template)?.len();
                    if sessions > 0 {
                        bail!(
                            "the source database has {sessions} open session(s); close them, or set \
                             postgres.on_busy_source = \"disconnect\" to abort them"
                        );
                    }
                }
                BusySourcePolicy::Disconnect => {
                    let (done, left) = self.terminate_sessions(template)?;
                    terminated += done;
                    stuck = left;
                }
            }
            match self.admin.batch_execute(&sql) {
                Ok(()) => {
                    if terminated > 0 {
                        self.warnings.push(format!(
                            "terminated {terminated} session(s) on the source database; \
                             their open transactions were aborted"
                        ));
                    }
                    return Ok(());
                }
                Err(e) if e.code() == Some(&SqlState::OBJECT_IN_USE) => {
                    if attempt == attempts {
                        let hint = if stuck.is_empty() {
                            String::new()
                        } else {
                            format!(" (could not terminate: {})", stuck.join(", "))
                        };
                        match policy {
                            BusySourcePolicy::Fail => bail!("the source database is in use"),
                            BusySourcePolicy::Disconnect => bail!(
                                "the source database is still in use after {attempts} attempts{hint}"
                            ),
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100 << (attempt - 1)));
                }
                Err(e) => return Err(e).context("cannot clone the source database"),
            }
        }
        bail!("the source database is in use")
    }

    /// Client sessions on `db` other than ours: `(pid, role)`. Sessions of roles
    /// the admin has no privileges of (a superuser, say) show a NULL
    /// `backend_type`; they still block a clone, so they are listed too.
    pub(super) fn sessions_on(&mut self, db: &str) -> Result<Vec<(i32, String)>> {
        Ok(self
            .admin
            .query(
                "SELECT pid, coalesce(usename, '')::text FROM pg_stat_activity \
                 WHERE datname = $1 AND pid <> pg_backend_pid() \
                   AND (backend_type IS NULL OR backend_type = 'client backend')",
                &[&db],
            )?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }

    /// Terminate other sessions on `db`. Returns how many were terminated and
    /// the sessions the admin is not allowed to signal.
    pub(super) fn terminate_sessions(&mut self, db: &str) -> Result<(usize, Vec<String>)> {
        let mut done = 0;
        let mut stuck = Vec::new();
        for (pid, user) in self.sessions_on(db)? {
            match self
                .admin
                .query_one("SELECT pg_terminate_backend($1)", &[&pid])
            {
                Ok(row) if row.get::<_, bool>(0) => done += 1,
                _ => stuck.push(format!("{user} (pid {pid})")),
            }
        }
        Ok((done, stuck))
    }

    /// `PUBLIC` loses `CONNECT, TEMPORARY`; only the project owner gets
    /// `CREATE, TEMPORARY` (no grant option, never `CONNECT`).
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

    pub(super) fn grant_connect(&mut self, db: &str, login: &str) -> Result<()> {
        self.step()?;
        self.admin
            .batch_execute(&format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                q(db),
                q(login)
            ))
            .with_context(|| format!("cannot grant {login} access to {db}"))
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
    /// branch's `cow` is replaced by the capability of the new data. The journal
    /// must still be this reset's (`cutover` at `generation`); returns the
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
                        updated_at = now() \
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
