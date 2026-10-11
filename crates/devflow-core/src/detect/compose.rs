//! Compose services: what runs, which engine, which ports, which volumes.
//!
//! The file is parsed as data. `${VAR:-default}` is resolved to its default
//! and other variables are left unresolved, never read from the environment.
//! `include:` entries and override files are not followed.
//!
//! Which files exist is read from the directory listing without following
//! symlinks: a compose or override file that is there but cannot be read (a
//! link out of the repository or a dangling one) still counts as a file
//! `docker compose` would merge, so the volume list is then incomplete.
use super::facts::{
    ComposeFacts, ComposeService, ComposeVolume, Engine, PublishedPort, VolumeMount,
};
use super::repo::Repo;
use super::yaml;
use serde_json::Value;

/// Compose file names in Docker Compose's own lookup order.
const FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];
/// Files Docker Compose merges over the main one by itself.
const OVERRIDE_FILES: [&str; 4] = [
    "compose.override.yaml",
    "compose.override.yml",
    "docker-compose.override.yaml",
    "docker-compose.override.yml",
];
const INIT_DIR: &str = "/docker-entrypoint-initdb.d";
const MAX_SERVICES: usize = 100;

pub(super) fn detect(repo: &Repo) -> Option<ComposeFacts> {
    let root = repo.list(".");
    let files: Vec<String> = FILES
        .iter()
        .filter(|f| root.has_file(f))
        .map(|f| (*f).to_string())
        .collect();
    let file = files.first()?.clone();
    let mut facts = ComposeFacts {
        file: file.clone(),
        files,
        project_name: None,
        services: Vec::new(),
        volumes: Vec::new(),
        volumes_complete: true,
        evidence: vec![file.clone()],
    };
    let text = repo.read_text(&file)?;
    let document = yaml::parse(repo, &file, &text)?;
    if document.get("include").is_some() {
        repo.warn(&file, "`include` entries are not followed");
        facts.volumes_complete = false;
    }
    if OVERRIDE_FILES.iter().any(|name| root.exists(name)) {
        // Merged over this file by `docker compose` itself, not read here.
        // Any entry of that name counts, even one the scan cannot open: a
        // link to a file outside the repository can rename a volume to one
        // the main checkout uses.
        facts.volumes_complete = false;
    }
    if FILES.iter().any(|name| root.hides(name)) {
        // A compose file that exists but cannot be read may be the one
        // `docker compose` actually uses, or define volumes of its own.
        repo.warn(
            &file,
            "another compose file exists that cannot be read; its volumes are unknown",
        );
        facts.volumes_complete = false;
    }
    if root.truncated() {
        // The listing stopped early: an override may be in the part not seen.
        facts.volumes_complete = false;
    }
    let (volumes, complete) = volumes(document.get("volumes"));
    facts.volumes = volumes;
    facts.volumes_complete &= complete;
    facts.project_name = document
        .get("name")
        .and_then(Value::as_str)
        .map(|n| expand(n).0)
        .filter(|n| !n.is_empty());
    if let Some(services) = document.get("services").and_then(Value::as_object) {
        // `serde_json::Map` is ordered by key unless a dependency turns on
        // `preserve_order`, so sort explicitly to stay deterministic.
        let mut names: Vec<&String> = services.keys().collect();
        names.sort();
        facts.services = names
            .into_iter()
            .take(MAX_SERVICES)
            .filter_map(|name| Some(service(&file, name, services.get(name)?)))
            .collect();
    }
    Some(facts)
}

/// The top-level `volumes:` definitions, and whether every one was understood.
///
/// A definition is a mapping or empty (`dbdata:`). Anything else, or a
/// `volumes:` that is not a mapping, is not something to guess about.
fn volumes(value: Option<&Value>) -> (Vec<ComposeVolume>, bool) {
    let Some(value) = value else {
        return (Vec::new(), true);
    };
    let Some(map) = value.as_object() else {
        return (Vec::new(), value.is_null());
    };
    let mut names: Vec<&String> = map.keys().collect();
    names.sort();
    let mut complete = true;
    let mut found = Vec::new();
    for name in names.into_iter().take(MAX_SERVICES) {
        let definition = &map[name];
        let (explicit_name, external) = match definition {
            Value::Null => (false, false),
            Value::Object(fields) => (
                fields.get("name").is_some_and(|n| !n.is_null()),
                match fields.get("external") {
                    None | Some(Value::Null) | Some(Value::Bool(false)) => false,
                    Some(Value::Bool(true)) | Some(Value::Object(_)) => true,
                    // `external: "false"`, a list, a number: unknown.
                    Some(_) => {
                        complete = false;
                        true
                    }
                },
            ),
            _ => {
                complete = false;
                (false, false)
            }
        };
        found.push(ComposeVolume {
            name: name.clone(),
            explicit_name,
            external,
        });
    }
    if map.len() > MAX_SERVICES {
        complete = false;
    }
    (found, complete)
}

fn service(file: &str, name: &str, value: &Value) -> ComposeService {
    let raw_image = value.get("image").and_then(Value::as_str);
    let (expanded, unresolved) = raw_image.map(expand).unzip();
    let image = match (raw_image, &expanded, &unresolved) {
        (Some(raw), Some(_), Some(missing)) if !missing.is_empty() => Some(raw.to_string()),
        (_, Some(e), _) => Some(e.clone()),
        _ => None,
    };
    let info = expanded
        .as_deref()
        .map(classify_image)
        .unwrap_or_else(ImageInfo::other);
    let build = value.get("build").is_some();

    let volumes: Vec<VolumeMount> = value
        .get("volumes")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_volume).collect())
        .unwrap_or_default();
    let init_scripts: Vec<String> = volumes
        .iter()
        .filter(|v| v.target == INIT_DIR || v.target.starts_with(&format!("{INIT_DIR}/")))
        .filter_map(|v| v.source.clone())
        .collect();

    let prefix = format!("{file}:services.{name}");
    let mut evidence = Vec::new();
    if image.is_some() {
        evidence.push(format!("{prefix}.image"));
    } else if build {
        evidence.push(format!("{prefix}.build"));
    }
    if !init_scripts.is_empty() {
        evidence.push(format!("{prefix}.volumes"));
    }
    let container_name = value
        .get("container_name")
        .and_then(Value::as_str)
        .map(|n| expand(n).0);
    if container_name.is_some() {
        evidence.push(format!("{prefix}.container_name"));
    }

    ComposeService {
        name: name.to_string(),
        image,
        build,
        engine: info.engine,
        flavor: info.flavor,
        tag: info.tag,
        major_version: info.major,
        ports: value
            .get("ports")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_port).collect())
            .unwrap_or_default(),
        container_name,
        volumes,
        init_scripts,
        environment_keys: environment_keys(value.get("environment")),
        profiles: value
            .get("profiles")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        evidence,
    }
}

fn environment_keys(value: Option<&Value>) -> Vec<String> {
    let mut keys: Vec<String> = match value {
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|entry| entry.split('=').next().unwrap_or(entry).trim().to_string())
            .filter(|k| !k.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    keys.sort();
    keys.dedup();
    keys
}

// -- variable interpolation -------------------------------------------------------

/// Resolve `${VAR:-default}`-style references. Returns the text and the names
/// of variables that had no default (replaced by nothing).
pub(super) fn expand(text: &str) -> (String, Vec<String>) {
    let mut unresolved = Vec::new();
    let out = expand_depth(text, &mut unresolved, 0);
    (out, unresolved)
}

fn expand_depth(text: &str, unresolved: &mut Vec<String>, depth: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        if let Some(after) = rest.strip_prefix('$') {
            out.push('$');
            rest = after;
        } else if let Some(braced) = rest.strip_prefix('{') {
            let Some(end) = matching_brace(braced) else {
                out.push('$');
                out.push('{');
                rest = braced;
                continue;
            };
            let inner = &braced[..end];
            rest = &braced[end + 1..];
            let name_len = inner
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(inner.len());
            let (name, operator) = inner.split_at(name_len);
            let default = ["-", ":-"].iter().find_map(|op| operator.strip_prefix(op));
            let replacement = ["+", ":+"].iter().find_map(|op| operator.strip_prefix(op));
            if let Some(default) = default {
                out.push_str(&if depth < 4 {
                    expand_depth(default, unresolved, depth + 1)
                } else {
                    default.to_string()
                });
            } else if replacement.is_none() {
                unresolved.push(name.to_string());
            }
        } else {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                out.push('$');
            } else {
                rest = &rest[name.len()..];
                unresolved.push(name);
            }
        }
    }
    out.push_str(rest);
    out
}

/// Index of the `}` closing a `${`, allowing nested `${...}` in defaults.
fn matching_brace(text: &str) -> Option<usize> {
    let mut depth = 1usize;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// -- ports and volumes ---------------------------------------------------------------

fn parse_port(value: &Value) -> Option<PublishedPort> {
    match value {
        Value::Number(n) => Some(PublishedPort {
            host_ip: None,
            host: None,
            host_variable: None,
            container: u16::try_from(n.as_u64()?).ok()?,
            protocol: "tcp".into(),
        }),
        Value::String(s) => parse_port_string(s),
        Value::Object(map) => {
            let container = port_number(map.get("target")?)?;
            let (host, host_variable) = match map.get("published") {
                Some(Value::String(s)) => {
                    let (text, missing) = expand(s);
                    (first_port(&text), missing.into_iter().next())
                }
                Some(other) => (port_number(other), None),
                None => (None, None),
            };
            Some(PublishedPort {
                host_ip: map
                    .get("host_ip")
                    .and_then(Value::as_str)
                    .map(|s| expand(s).0),
                host,
                host_variable,
                container,
                protocol: map
                    .get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("tcp")
                    .to_string(),
            })
        }
        _ => None,
    }
}

fn port_number(value: &Value) -> Option<u16> {
    match value {
        Value::Number(n) => u16::try_from(n.as_u64()?).ok(),
        Value::String(s) => first_port(&expand(s).0),
        _ => None,
    }
}

/// First port of `8000` or a range `8000-8010`.
fn first_port(text: &str) -> Option<u16> {
    text.trim().split('-').next()?.trim().parse().ok()
}

/// `[IP:][HOST:]CONTAINER[/PROTOCOL]`, IPv6 addresses bracketed.
fn parse_port_string(text: &str) -> Option<PublishedPort> {
    let (spec, protocol) = match text.rsplit_once('/') {
        Some((spec, proto)) if matches!(proto, "tcp" | "udp" | "sctp") => (spec, proto),
        _ => (text, "tcp"),
    };
    let (expanded, missing) = expand(spec);
    let mut host_ip = None;
    let mut rest = expanded.as_str();
    if let Some(stripped) = rest.strip_prefix('[') {
        let (ip, tail) = stripped.split_once(']')?;
        host_ip = Some(ip.to_string());
        rest = tail.strip_prefix(':').unwrap_or(tail);
    }
    let parts: Vec<&str> = rest.split(':').collect();
    let (ip, host, container) = match parts.as_slice() {
        [container] => (None, "", *container),
        [host, container] => (None, *host, *container),
        [ip, host, container] => (Some(*ip), *host, *container),
        _ => return None,
    };
    if host_ip.is_none() {
        host_ip = ip.filter(|i| !i.is_empty()).map(str::to_string);
    }
    let host_port = first_port(host);
    Some(PublishedPort {
        host_ip,
        host: host_port,
        host_variable: if host_port.is_none() && !host.is_empty()
            || (host.is_empty() && !missing.is_empty() && parts.len() > 1)
        {
            missing.into_iter().next()
        } else {
            None
        },
        container: first_port(container)?,
        protocol: protocol.to_string(),
    })
}

fn parse_volume(value: &Value) -> Option<VolumeMount> {
    match value {
        Value::String(s) => {
            let (text, _) = expand(s);
            let mut parts = text.split(':');
            let first = parts.next()?;
            match parts.next() {
                None => Some(VolumeMount {
                    source: None,
                    target: first.to_string(),
                    bind: false,
                }),
                Some(target) => Some(VolumeMount {
                    source: Some(first.to_string()).filter(|s| !s.is_empty()),
                    target: target.to_string(),
                    bind: first.contains('/') || first.starts_with(['.', '~']),
                }),
            }
        }
        Value::Object(map) => {
            let target = map.get("target")?.as_str()?.to_string();
            let source = map
                .get("source")
                .and_then(Value::as_str)
                .map(|s| expand(s).0);
            let bind = map.get("type").and_then(Value::as_str) == Some("bind");
            Some(VolumeMount {
                source,
                target,
                bind,
            })
        }
        _ => None,
    }
}

// -- images ------------------------------------------------------------------------------

struct ImageInfo {
    engine: Engine,
    flavor: Option<String>,
    tag: Option<String>,
    major: Option<u32>,
}

impl ImageInfo {
    fn other() -> Self {
        Self {
            engine: Engine::Other,
            flavor: None,
            tag: None,
            major: None,
        }
    }
}

/// Guess the engine, variant, tag and major version from an image reference.
fn classify_image(image: &str) -> ImageInfo {
    let image = image.trim().to_ascii_lowercase();
    let without_digest = image.split('@').next().unwrap_or("");
    // The tag follows the last `:` after the last `/` (a registry port comes earlier).
    let last_slash = without_digest.rfind('/').map_or(0, |i| i + 1);
    let (repository, tag) = match without_digest[last_slash..].rfind(':') {
        Some(i) => (
            &without_digest[..last_slash + i],
            Some(without_digest[last_slash + i + 1..].to_string()).filter(|t| !t.is_empty()),
        ),
        None => (without_digest, None),
    };
    let mut components: Vec<&str> = repository.split('/').filter(|c| !c.is_empty()).collect();
    if components.len() > 1
        && components
            .first()
            .is_some_and(|c| c.contains('.') || c.contains(':') || *c == "localhost")
    {
        components.remove(0); // registry host
    }
    let name = components.last().copied().unwrap_or("");
    let owner = if components.len() > 1 {
        components[components.len() - 2]
    } else {
        ""
    };

    let (engine, flavor) = match (owner, name) {
        ("supabase", "postgres") => (Engine::Postgres, Some("supabase")),
        (_, "postgres" | "postgresql" | "postgresql-repmgr" | "postgresql-ha" | "postgres-ha") => {
            (Engine::Postgres, None)
        }
        (_, "postgis") => (Engine::Postgres, Some("postgis")),
        (_, "pgvector") => (Engine::Postgres, Some("pgvector")),
        (_, "pgvecto-rs") => (Engine::Postgres, Some("pgvecto-rs")),
        (_, "timescaledb" | "timescaledb-ha") => (Engine::Postgres, Some("timescaledb")),
        (_, "citus") => (Engine::Postgres, Some("citus")),
        (_, "paradedb") => (Engine::Postgres, Some("paradedb")),
        (_, "mysql" | "mysql-server" | "percona-server" | "percona") => (Engine::Mysql, None),
        (_, "mariadb") => (Engine::Mariadb, None),
        (_, "redis" | "redis-stack" | "redis-stack-server" | "redis-cluster") => {
            (Engine::Redis, None)
        }
        (_, "valkey" | "valkey-bundle") => (Engine::Valkey, None),
        (_, "memcached") => (Engine::Memcached, None),
        (_, "mongo" | "mongodb" | "mongodb-community-server" | "mongodb-enterprise-server") => {
            (Engine::Mongodb, None)
        }
        (_, "clickhouse-server" | "clickhouse") => (Engine::Clickhouse, None),
        (_, "minio") => (Engine::Minio, None),
        (_, "rabbitmq") => (Engine::Rabbitmq, None),
        (_, "kafka" | "cp-kafka" | "cp-server") => (Engine::Kafka, None),
        (_, "redpanda") => (Engine::Redpanda, None),
        (_, "opensearch") => (Engine::Opensearch, None),
        (_, "elasticsearch") => (Engine::Elasticsearch, None),
        (_, "meilisearch") => (Engine::Meilisearch, None),
        (_, "mailpit") => (Engine::Mailpit, None),
        (_, "localstack" | "localstack-pro") => (Engine::Localstack, None),
        ("mssql", "server") | (_, "mssql-server") => (Engine::SqlServer, None),
        _ => (Engine::Other, None),
    };
    let major = if engine == Engine::Other {
        None
    } else {
        tag.as_deref().and_then(|t| major_version(t, engine))
    };
    ImageInfo {
        engine,
        flavor: flavor.map(str::to_string),
        tag,
        major,
    }
}

/// Major version from a tag: `16-alpine` is 16, `latest-pg17` and `pg17.1-ts2` are 17.
fn major_version(tag: &str, engine: Engine) -> Option<u32> {
    fn leading_number(token: &str) -> Option<u32> {
        let digits: String = token.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }
    let tokens: Vec<&str> = tag.split(['-', '_']).collect();
    let major = (engine == Engine::Postgres)
        .then(|| {
            tokens
                .iter()
                .find_map(|t| t.strip_prefix("pg").and_then(leading_number))
        })
        .flatten()
        .or_else(|| {
            tokens
                .iter()
                .find(|t| t.starts_with(|c: char| c.is_ascii_digit()))
                .and_then(|t| leading_number(t))
        })?;
    // A bare date or build number is not a database version.
    if engine == Engine::Postgres && !(8..=40).contains(&major) {
        return None;
    }
    (major < 1000).then_some(major)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(image: &str) -> (Engine, Option<String>, Option<u32>) {
        let info = classify_image(image);
        (info.engine, info.flavor, info.major)
    }

    #[test]
    fn top_level_volumes_say_which_ones_a_project_name_cannot_isolate() {
        let document: Value = serde_json::json!({
            "plain": null,
            "empty": {},
            "driver": {"driver": "local"},
            "named": {"name": "shop-data"},
            "outside": {"external": true},
            "legacy": {"external": {"name": "old"}},
            "not-external": {"external": false}
        });
        let (found, complete) = volumes(Some(&document));
        assert!(complete);
        let shared: Vec<(&str, bool, bool)> = found
            .iter()
            .filter(|v| v.shared())
            .map(|v| (v.name.as_str(), v.explicit_name, v.external))
            .collect();
        assert_eq!(
            shared,
            vec![
                ("legacy", false, true),
                ("named", true, false),
                ("outside", false, true)
            ]
        );
        assert_eq!(found.len(), 7);
        assert_eq!(volumes(None), (Vec::new(), true));
        assert_eq!(volumes(Some(&Value::Null)), (Vec::new(), true));
        // Not understood: reported, never guessed.
        assert!(!volumes(Some(&serde_json::json!(["a"]))).1);
        assert!(!volumes(Some(&serde_json::json!({"a": "nope"}))).1);
        assert!(!volumes(Some(&serde_json::json!({"a": {"external": "maybe"}}))).1);
    }

    #[test]
    fn postgres_images_and_versions() {
        assert_eq!(
            classify("postgres:16-alpine"),
            (Engine::Postgres, None, Some(16))
        );
        assert_eq!(
            classify("postgres:16.2"),
            (Engine::Postgres, None, Some(16))
        );
        assert_eq!(classify("postgres"), (Engine::Postgres, None, None));
        assert_eq!(classify("postgres:latest"), (Engine::Postgres, None, None));
        assert_eq!(
            classify("postgis/postgis:15-3.4"),
            (Engine::Postgres, Some("postgis".into()), Some(15))
        );
        assert_eq!(
            classify("pgvector/pgvector:pg17"),
            (Engine::Postgres, Some("pgvector".into()), Some(17))
        );
        assert_eq!(
            classify("ankane/pgvector:v0.5.1"),
            (Engine::Postgres, Some("pgvector".into()), None)
        );
        assert_eq!(
            classify("timescale/timescaledb:2.15.2-pg16"),
            (Engine::Postgres, Some("timescaledb".into()), Some(16))
        );
        assert_eq!(
            classify("timescale/timescaledb-ha:pg16.3-ts2.15.2"),
            (Engine::Postgres, Some("timescaledb".into()), Some(16))
        );
        assert_eq!(
            classify("registry.example.com:5000/mirror/postgres:15"),
            (Engine::Postgres, None, Some(15))
        );
        assert_eq!(
            classify("bitnami/postgresql:16.2.0-debian-12-r0"),
            (Engine::Postgres, None, Some(16))
        );
        assert_eq!(
            classify("supabase/postgres:15.1.0.147"),
            (Engine::Postgres, Some("supabase".into()), Some(15))
        );
        assert_eq!(
            classify("postgres@sha256:abcdef"),
            (Engine::Postgres, None, None)
        );
    }

    #[test]
    fn other_engines() {
        assert_eq!(classify("mysql:8.0").0, Engine::Mysql);
        assert_eq!(classify("mariadb:11").0, Engine::Mariadb);
        assert_eq!(classify("redis:7-alpine"), (Engine::Redis, None, Some(7)));
        assert_eq!(classify("valkey/valkey:8").0, Engine::Valkey);
        assert_eq!(classify("memcached:1.6").0, Engine::Memcached);
        assert_eq!(classify("mongo:7").0, Engine::Mongodb);
        assert_eq!(
            classify("clickhouse/clickhouse-server:24.3").0,
            Engine::Clickhouse
        );
        assert_eq!(classify("quay.io/minio/minio").0, Engine::Minio);
        assert_eq!(classify("rabbitmq:3-management").0, Engine::Rabbitmq);
        assert_eq!(classify("confluentinc/cp-kafka:7.6.0").0, Engine::Kafka);
        assert_eq!(
            classify("redpandadata/redpanda:v24.1.1").0,
            Engine::Redpanda
        );
        assert_eq!(
            classify("opensearchproject/opensearch:2").0,
            Engine::Opensearch
        );
        assert_eq!(
            classify("docker.elastic.co/elasticsearch/elasticsearch:8.13.0").0,
            Engine::Elasticsearch
        );
        assert_eq!(classify("getmeili/meilisearch:v1.8").0, Engine::Meilisearch);
        assert_eq!(classify("axllent/mailpit").0, Engine::Mailpit);
        assert_eq!(classify("localstack/localstack").0, Engine::Localstack);
        assert_eq!(
            classify("mcr.microsoft.com/mssql/server:2022-latest").0,
            Engine::SqlServer
        );
        assert_eq!(classify("nginx:alpine").0, Engine::Other);
        assert_eq!(classify("myapp-web").0, Engine::Other);
    }

    #[test]
    fn interpolation_uses_defaults_and_reports_the_rest() {
        assert_eq!(
            expand("postgres:${PG_VERSION:-16}"),
            ("postgres:16".into(), vec![])
        );
        assert_eq!(expand("${A:-${B:-x}}"), ("x".into(), vec![]));
        assert_eq!(
            expand("${MISSING}:5432"),
            (":5432".into(), vec!["MISSING".into()])
        );
        assert_eq!(expand("$$HOME"), ("$HOME".into(), vec![]));
        assert_eq!(expand("a$"), ("a$".into(), vec![]));
    }

    #[test]
    fn port_forms() {
        let p = parse_port_string("127.0.0.1:5433:5432").unwrap();
        assert_eq!(
            (p.host_ip.as_deref(), p.host, p.container),
            (Some("127.0.0.1"), Some(5433), 5432)
        );
        let p = parse_port_string("5432").unwrap();
        assert_eq!((p.host, p.container), (None, 5432));
        let p = parse_port_string("8000-8010:8000-8010/udp").unwrap();
        assert_eq!(
            (p.host, p.container, p.protocol.as_str()),
            (Some(8000), 8000, "udp")
        );
        let p = parse_port_string("${DB_PORT}:5432").unwrap();
        assert_eq!(
            (p.host, p.host_variable.as_deref()),
            (None, Some("DB_PORT"))
        );
        let p = parse_port_string("${DB_PORT:-5433}:5432").unwrap();
        assert_eq!((p.host, p.host_variable), (Some(5433), None));
        let p = parse_port_string("[::1]:6379:6379").unwrap();
        assert_eq!((p.host_ip.as_deref(), p.host), (Some("::1"), Some(6379)));
    }
}
