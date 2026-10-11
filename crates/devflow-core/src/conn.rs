//! PostgreSQL connection parameters parsed the way libpq parses them, so a
//! workspace's `DATABASE_URL`, its generated `PG*` variables and the `psql`
//! devflow starts all reach the same server with the same options.
//!
//! Accepts `postgres://` / `postgresql://` URIs and `key=value` conninfo
//! strings. Later settings win (query parameters override URI components and
//! earlier duplicates). Keywords that can only be expressed in the string
//! (`sslpassword`, `replication`, multiple hosts) are rejected because the
//! generated environment could not reproduce them. Keywords libpq does not
//! know (ORM options such as Prisma's `schema=`) are kept as application
//! options: they stay in `DATABASE_URL` but never reach `PG*` variables.
use anyhow::{Result, bail};
use percent_encoding::percent_decode_str;

/// libpq keywords and the environment variables that express them.
const KEYWORDS: &[(&str, &str)] = &[
    ("host", "PGHOST"),
    ("hostaddr", "PGHOSTADDR"),
    ("port", "PGPORT"),
    ("dbname", "PGDATABASE"),
    ("user", "PGUSER"),
    ("password", "PGPASSWORD"),
    ("passfile", "PGPASSFILE"),
    ("channel_binding", "PGCHANNELBINDING"),
    ("connect_timeout", "PGCONNECT_TIMEOUT"),
    ("client_encoding", "PGCLIENTENCODING"),
    ("options", "PGOPTIONS"),
    ("application_name", "PGAPPNAME"),
    ("sslmode", "PGSSLMODE"),
    ("sslnegotiation", "PGSSLNEGOTIATION"),
    ("sslcompression", "PGSSLCOMPRESSION"),
    ("sslcert", "PGSSLCERT"),
    ("sslkey", "PGSSLKEY"),
    ("sslcertmode", "PGSSLCERTMODE"),
    ("sslrootcert", "PGSSLROOTCERT"),
    ("sslcrl", "PGSSLCRL"),
    ("sslcrldir", "PGSSLCRLDIR"),
    ("sslsni", "PGSSLSNI"),
    ("requirepeer", "PGREQUIREPEER"),
    ("require_auth", "PGREQUIREAUTH"),
    ("ssl_min_protocol_version", "PGSSLMINPROTOCOLVERSION"),
    ("ssl_max_protocol_version", "PGSSLMAXPROTOCOLVERSION"),
    ("min_protocol_version", "PGMINPROTOCOLVERSION"),
    ("max_protocol_version", "PGMAXPROTOCOLVERSION"),
    ("gssencmode", "PGGSSENCMODE"),
    ("krbsrvname", "PGKRBSRVNAME"),
    ("gsslib", "PGGSSLIB"),
    ("gssdelegation", "PGGSSDELEGATION"),
    ("target_session_attrs", "PGTARGETSESSIONATTRS"),
    ("load_balance_hosts", "PGLOADBALANCEHOSTS"),
];

/// Keywords libpq accepts that only tune the client socket; they have no
/// environment variable, and dropping them never changes which server or
/// credentials are used.
const SOCKET_ONLY: &[&str] = &[
    "keepalives",
    "keepalives_idle",
    "keepalives_interval",
    "keepalives_count",
    "tcp_user_timeout",
    "fallback_application_name",
];

/// Environment variables that select or authenticate a connection. They are
/// cleared before the generated values are applied, so nothing inherited
/// from the caller's shell can redirect a hook or `psql`.
pub fn libpq_env_vars() -> impl Iterator<Item = &'static str> {
    KEYWORDS.iter().map(|(_, env)| *env).chain([
        "PGSERVICE",
        "PGSERVICEFILE",
        "PGSYSCONFDIR",
        "PGREQUIRESSL",
        "PGREALM",
        "PGTZ",
        "PGDATESTYLE",
        "PGGEQO",
    ])
}

/// Keywords whose values are credentials.
pub fn is_secret_keyword(keyword: &str) -> bool {
    matches!(keyword, "password" | "sslpassword")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    /// Effective libpq keyword values (each keyword once, last setting wins).
    values: Vec<(String, String)>,
    /// Options libpq does not know, for the application only.
    app_options: Vec<(String, String)>,
}

impl Params {
    pub fn parse(input: &str) -> Result<Params> {
        let input = input.trim();
        let pairs = if let Some(rest) = input
            .strip_prefix("postgresql://")
            .or_else(|| input.strip_prefix("postgres://"))
        {
            parse_uri(rest)?
        } else if input.contains("://") {
            bail!("unsupported connection URI scheme (expected postgres:// or postgresql://)");
        } else {
            parse_conninfo(input)?
        };
        let mut params = Params {
            values: Vec::new(),
            app_options: Vec::new(),
        };
        for (key, value) in pairs {
            params.set(&key, value)?;
        }
        Ok(params)
    }

    fn set(&mut self, key: &str, value: String) -> Result<()> {
        match key {
            "sslpassword" => bail!("sslpassword is not supported in devflow connection URLs"),
            "replication" => bail!("replication connections are not supported"),
            // Aliases some drivers understand as "require TLS". Dropping them
            // would silently weaken the generated PG* connection.
            "ssl" | "requiressl" => bail!("'{key}' is not supported; use sslmode=require"),
            // As PGSERVICE, a service file's values would override the
            // explicit ones instead of the other way round.
            "service" => bail!("service= is not supported; spell the connection out in the URL"),
            "host" | "hostaddr" | "port" if value.contains(',') => {
                bail!("connection URLs with several hosts are not supported")
            }
            k if KEYWORDS.iter().any(|(kw, _)| *kw == k) || SOCKET_ONLY.contains(&k) => {}
            _ => {
                self.app_options.retain(|(k, _)| k != key);
                self.app_options.push((key.to_string(), value));
                return Ok(());
            }
        }
        self.values.retain(|(k, _)| k != key);
        self.values.push((key.to_string(), value));
        Ok(())
    }

    pub fn get(&self, keyword: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| k == keyword)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    /// Options libpq does not know (kept for the application, never in `PG*`).
    pub fn app_options(&self) -> &[(String, String)] {
        &self.app_options
    }

    pub fn password(&self) -> Option<&str> {
        self.get("password")
    }

    /// The `PG*` variables expressing these parameters (password included).
    pub fn env(&self) -> Vec<(&'static str, String)> {
        KEYWORDS
            .iter()
            .filter_map(|(keyword, env)| self.get(keyword).map(|v| (*env, v.to_string())))
            .collect()
    }

    /// [`Params::env`] without `PGPASSWORD`, for printing.
    pub fn env_without_password(&self) -> Vec<(&'static str, String)> {
        self.env()
            .into_iter()
            .filter(|(env, _)| *env != "PGPASSWORD")
            .collect()
    }
}

/// `[user[:password]@][host][:port][/dbname][?key=value&...]`, scanned the way
/// libpq scans it: credentials run to the first `@` unless a `/` comes first,
/// `#` is an ordinary character (libpq URIs have no fragments), and every part
/// is percent-decoded, parameter names included.
fn parse_uri(rest: &str) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut s = rest;
    if let Some(at) = s.find(['@', '/']).filter(|&i| s.as_bytes()[i] == b'@') {
        let userinfo = &s[..at];
        let (user, password) = match userinfo.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (userinfo, None),
        };
        if !user.is_empty() {
            pairs.push(("user".into(), decode(user)?));
        }
        if let Some(password) = password {
            pairs.push(("password".into(), decode(password)?));
        }
        s = &s[at + 1..];
    }
    let host_end = s.find(['/', '?']).unwrap_or(s.len());
    let hostport = &s[..host_end];
    s = &s[host_end..];
    if hostport.contains(',') {
        bail!("connection URLs with several hosts are not supported");
    }
    let (host, port) = if let Some(bracketed) = hostport.strip_prefix('[') {
        let (host, after) = bracketed
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("unterminated IPv6 address in connection URL"))?;
        (host, after.strip_prefix(':'))
    } else {
        match hostport.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (hostport, None),
        }
    };
    if !host.is_empty() {
        pairs.push(("host".into(), decode(host)?));
    }
    if let Some(port) = port.filter(|p| !p.is_empty()) {
        pairs.push(("port".into(), decode(port)?));
    }
    if let Some(after) = s.strip_prefix('/') {
        let db_end = after.find('?').unwrap_or(after.len());
        if db_end > 0 {
            pairs.push(("dbname".into(), decode(&after[..db_end])?));
        }
        s = &after[db_end..];
    }
    let query = s.strip_prefix('?').unwrap_or("");
    for item in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = item
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("connection URL parameter without a value"))?;
        pairs.push((decode(key)?, decode(value)?));
    }
    Ok(pairs)
}

fn decode(text: &str) -> Result<String> {
    Ok(percent_decode_str(text).decode_utf8()?.into_owned())
}

/// `key = value ...`; values may be single-quoted with `\'` and `\\` escapes.
fn parse_conninfo(input: &str) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c.is_whitespace() {
                break;
            }
            key.push(c);
            chars.next();
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.next() != Some('=') {
            bail!("missing '=' after '{key}' in connection string");
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'\'') {
            chars.next();
            loop {
                match chars.next() {
                    Some('\\') => value.extend(chars.next()),
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => bail!("unterminated quoted value for '{key}'"),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                chars.next();
                if c == '\\' {
                    value.extend(chars.next());
                } else {
                    value.push(c);
                }
            }
        }
        pairs.push((key, value));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(url: &str) -> Vec<(&'static str, String)> {
        Params::parse(url).unwrap().env()
    }

    #[test]
    fn uri_components_and_query_follow_libpq_precedence() {
        let p = Params::parse(
            "postgresql://app:p%40ss@db.internal:5433/shop?port=5434&sslmode=disable&sslmode=require&%70assword=override", // kingfisher:ignore
        )
        .unwrap();
        assert_eq!(p.get("user"), Some("app"));
        assert_eq!(p.get("host"), Some("db.internal"));
        assert_eq!(p.get("port"), Some("5434"));
        assert_eq!(p.get("dbname"), Some("shop"));
        assert_eq!(p.get("sslmode"), Some("require"));
        assert_eq!(p.password(), Some("override"));
    }

    #[test]
    fn hash_signs_are_ordinary_characters_like_in_libpq() {
        let p = Params::parse("postgres://u:abc#def@localhost/db#x?password=a#b").unwrap();
        assert_eq!(p.get("user"), Some("u"));
        assert_eq!(p.get("host"), Some("localhost"));
        assert_eq!(p.get("dbname"), Some("db#x"));
        assert_eq!(p.password(), Some("a#b"));
        let p = Params::parse("postgres://u:abc#def@localhost/db").unwrap();
        assert_eq!(p.password(), Some("abc#def"));
        assert_eq!(p.get("port"), None);
    }

    #[test]
    fn tls_aliases_are_refused_rather_than_dropped() {
        for alias in ["ssl=true", "requiressl=1"] {
            let error = Params::parse(&format!("postgres://u@h/d?{alias}")).unwrap_err();
            assert!(error.to_string().contains("sslmode=require"), "{error}");
        }
    }

    #[test]
    fn hostaddr_and_ipv6_hosts_are_kept() {
        let e = env("postgres://u@[::1]:6000/d?hostaddr=192.0.2.44");
        assert!(e.contains(&("PGHOST", "::1".into())));
        assert!(e.contains(&("PGPORT", "6000".into())));
        assert!(e.contains(&("PGHOSTADDR", "192.0.2.44".into())));
    }

    #[test]
    fn conninfo_supports_spaces_quotes_and_escapes() {
        let p = Params::parse(r"host=h  password = 'it\'s secret' dbname=d").unwrap();
        assert_eq!(p.password(), Some("it's secret"));
        assert_eq!(p.get("host"), Some("h"));
        assert_eq!(p.get("dbname"), Some("d"));
    }

    #[test]
    fn unsupported_forms_are_rejected() {
        for bad in [
            "postgres://u@h1,h2/d",
            "postgres://u@h/d?sslpassword=x",
            "postgres://u@h/d?replication=database",
            "postgres://u@h/d?service=prod",
            "mysql://u@h/d",
            "host=h password",
        ] {
            assert!(Params::parse(bad).is_err(), "{bad} must be rejected");
        }
        let prisma = Params::parse("postgres://u@h/d?schema=public&keepalives=1").unwrap();
        assert_eq!(
            prisma.app_options(),
            [("schema".to_string(), "public".to_string())]
        );
        assert!(prisma.env().iter().all(|(k, _)| *k != "PGSCHEMA"));
    }

    #[test]
    fn every_libpq_connection_variable_is_cleared() {
        let cleared: Vec<_> = libpq_env_vars().collect();
        // Enumerated from the PostgreSQL 18 libpq environment-variable docs.
        for var in [
            "PGHOST",
            "PGHOSTADDR",
            "PGPORT",
            "PGDATABASE",
            "PGUSER",
            "PGPASSWORD",
            "PGPASSFILE",
            "PGSERVICE",
            "PGSERVICEFILE",
            "PGOPTIONS",
            "PGAPPNAME",
            "PGSSLMODE",
            "PGREQUIRESSL",
            "PGSSLNEGOTIATION",
            "PGSSLCOMPRESSION",
            "PGSSLCERT",
            "PGSSLKEY",
            "PGSSLCERTMODE",
            "PGSSLROOTCERT",
            "PGSSLCRL",
            "PGSSLCRLDIR",
            "PGSSLSNI",
            "PGREQUIREPEER",
            "PGSSLMINPROTOCOLVERSION",
            "PGSSLMAXPROTOCOLVERSION",
            "PGMINPROTOCOLVERSION",
            "PGMAXPROTOCOLVERSION",
            "PGGSSENCMODE",
            "PGKRBSRVNAME",
            "PGGSSLIB",
            "PGGSSDELEGATION",
            "PGCONNECT_TIMEOUT",
            "PGCLIENTENCODING",
            "PGTARGETSESSIONATTRS",
            "PGLOADBALANCEHOSTS",
            "PGCHANNELBINDING",
            "PGREQUIREAUTH",
            "PGDATESTYLE",
            "PGTZ",
            "PGGEQO",
            "PGSYSCONFDIR",
            "PGREALM",
        ] {
            assert!(cleared.contains(&var), "{var} must be cleared");
        }
    }

    #[test]
    fn password_free_env_omits_only_the_password() {
        let p = Params::parse("postgres://u:pw@h/d").unwrap(); // kingfisher:ignore
        assert!(p.env().iter().any(|(k, _)| *k == "PGPASSWORD"));
        assert!(
            p.env_without_password()
                .iter()
                .all(|(k, _)| *k != "PGPASSWORD")
        );
    }
}
