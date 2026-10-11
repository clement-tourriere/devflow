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

/// The `PG*` variables a connection URL can express, in libpq's table order.
pub fn pg_env_names() -> impl Iterator<Item = &'static str> {
    KEYWORDS.iter().map(|(_, env)| *env)
}

/// Everything except the RFC 3986 unreserved characters is percent-encoded.
const URL_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// `text` as one component (user, password, database name) of a URL.
pub fn url_component(text: &str) -> String {
    percent_encoding::utf8_percent_encode(text, URL_COMPONENT).to_string()
}

/// `host` as the host part of a URL: IPv6 addresses are bracketed.
pub fn url_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// `url` (`scheme://authority/database?options`) with another database: the
/// credentials, endpoint and options are kept byte for byte, `database` is
/// percent-encoded. `None` when `url` is not a URL.
pub fn url_with_database(url: &str, database: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    // The authority ends at the first `/` or `?` (a `/` inside it would be
    // percent-encoded).
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let options = tail.find('?').map_or("", |at| &tail[at..]);
    Some(format!(
        "{scheme}://{authority}/{}{options}",
        url_component(database)
    ))
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
            // The keyword is not named: in a string with a typo it may be the
            // password itself.
            bail!("a keyword in the connection string has no '=' and value");
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
                    None => bail!("a quoted value in the connection string is not terminated"),
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
    fn a_url_keeps_everything_but_its_database() {
        let with = |url: &str, db: &str| url_with_database(url, db);
        assert_eq!(
            with("postgres://dfl_a:p%40ss@127.0.0.1:5432/df_x", "df_x_1").as_deref(),
            Some("postgres://dfl_a:p%40ss@127.0.0.1:5432/df_x_1") // kingfisher:ignore
        );
        // Options survive, an IPv6 host is left alone, the name is encoded.
        assert_eq!(
            with("postgres://u:p@[::1]:1/d?sslmode=require&x=a/b", "my db").as_deref(),
            Some("postgres://u:p@[::1]:1/my%20db?sslmode=require&x=a/b") // kingfisher:ignore
        );
        // No database in the URL, with and without options.
        assert_eq!(
            with("postgres://u@h", "d").as_deref(),
            Some("postgres://u@h/d")
        );
        assert_eq!(
            with("postgres://u@h?sslmode=disable", "d").as_deref(),
            Some("postgres://u@h/d?sslmode=disable")
        );
        assert_eq!(with("not a url", "d"), None);
        // The parser reads back what was written.
        let url = with("postgres://u:p@h:7/d", "other_1").unwrap(); // kingfisher:ignore
        assert_eq!(Params::parse(&url).unwrap().get("dbname"), Some("other_1"));
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
    fn malformed_connection_strings_are_refused_without_their_text() {
        for text in [
            "REVIEWSECRET",
            "host=h REVIEWSECRET",
            "host=h password='REVIEWSECRET",
            "postgres://u@h/db?REVIEWSECRET",
            "postgres://u@[REVIEWSECRET/db",
        ] {
            let error = Params::parse(text).unwrap_err();
            let shown = format!("{error} | {error:#} | {error:?}");
            assert!(!shown.contains("REVIEWSECRET"), "{text}\n{shown}");
        }
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
    fn url_components_are_encoded_and_ipv6_hosts_bracketed() {
        assert_eq!(url_component("simple-name_1.x~"), "simple-name_1.x~");
        assert_eq!(
            url_component("p@ss/w:rd#?% é"),
            "p%40ss%2Fw%3Ard%23%3F%25%20%C3%A9"
        );
        assert_eq!(url_host("db.internal"), "db.internal");
        assert_eq!(url_host("::1"), "[::1]");
        assert_eq!(url_host("[::1]"), "[::1]");
        assert!(pg_env_names().any(|n| n == "PGPASSWORD"));
        assert!(pg_env_names().all(|n| n.starts_with("PG")));
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
