//! Compose services devflow branches itself: Valkey, Redis and MySQL 8.4.
//!
//! A compose service with one of those images becomes a proposal item
//! `service:<key>` for a `[services.<key>]` table. The compose image is kept
//! when devflow's profile can run it; otherwise the item uses devflow's
//! default image and says why. What devflow does not propose is said in one
//! of two ways:
//!
//! - *not proposed automatically*: devflow could branch it, but a person has
//!   to decide (modules in the image, an unpinned MySQL tag, a second MySQL
//!   service, a compose profile). The warning points to the manual
//!   configuration in the README.
//! - *unsupported*: outside what the profile can run (another MySQL version,
//!   Percona, a Redis cluster).
//!
//! Both stay in the proposed compose hooks, which only start what devflow
//! does not branch.
use super::compose::valid_service_name;
use super::plan::{Builder, Pushed, Spec};
use super::{Confidence, ItemKind, PatchOp};
use crate::config::{
    self, Engine as Branch, MYSQL_IMAGE, MYSQL_URL_ENV, REDIS_IMAGE, SERVICE_SUBCOMMANDS,
    VALKEY_IMAGE,
};
use crate::detect::{ComposeService, EnvCategory};
use serde_json::json;
use std::collections::BTreeSet;

/// Where a person finds the manual configuration of each family.
const README_CACHE: &str = "README, \u{201c}Valkey and Redis branches\u{201d}";
const README_MYSQL: &str = "README, \u{201c}MySQL branches (preview)\u{201d}";

/// The variables the application is likely to read a cache URL from, in the
/// order devflow prefers them.
const CACHE_URLS: [&str; 4] = ["REDIS_URL", "VALKEY_URL", "CACHE_URL", "REDIS_URI"];
/// The same for MySQL.
const MYSQL_URLS: [&str; 2] = ["DATABASE_URL", "MYSQL_URL"];

/// An image reference taken apart, enough to recognise what it is.
#[derive(Debug, PartialEq, Eq)]
struct ImageRef {
    /// Docker Hub: no registry host, or one of its names.
    hub: bool,
    /// The path without registry and without a leading `library/`.
    path: Vec<String>,
    tag: Option<String>,
}

impl ImageRef {
    fn parse(image: &str) -> Self {
        let image = image.trim().to_ascii_lowercase();
        let without_digest = image.split('@').next().unwrap_or("");
        let last_slash = without_digest.rfind('/').map_or(0, |i| i + 1);
        let (repository, tag) = match without_digest[last_slash..].rfind(':') {
            Some(i) => (
                &without_digest[..last_slash + i],
                Some(without_digest[last_slash + i + 1..].to_string()).filter(|t| !t.is_empty()),
            ),
            None => (without_digest, None),
        };
        let mut path: Vec<String> = repository
            .split('/')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        let mut hub = true;
        if path.len() > 1
            && path
                .first()
                .is_some_and(|p| p.contains('.') || p.contains(':') || p == "localhost")
        {
            let host = path.remove(0);
            hub = matches!(
                host.as_str(),
                "docker.io"
                    | "index.docker.io"
                    | "registry-1.docker.io"
                    | "registry.hub.docker.com"
            );
        }
        if hub && path.len() > 1 && path[0] == "library" {
            path.remove(0);
        }
        Self { hub, path, tag }
    }

    fn is(&self, path: &[&str]) -> bool {
        self.hub
            && self
                .path
                .iter()
                .map(String::as_str)
                .eq(path.iter().copied())
    }

    /// The last path component: the image's own name.
    fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    /// Bitnami's namespaces on Docker Hub.
    fn bitnami(&self, name: &str) -> bool {
        self.hub
            && self.path.len() == 2
            && matches!(
                self.path[0].as_str(),
                "bitnami" | "bitnamilegacy" | "bitnamisecure"
            )
            && self.path[1] == name
    }

    /// `(major, minor)` of the first version-like token of the tag
    /// (`7.2-alpine` is 7.2, `8-alpine` is 8, `alpine` has none).
    fn version(&self) -> Option<(u32, Option<u32>)> {
        let tag = self.tag.as_deref()?;
        let token = tag
            .split(['-', '_'])
            .map(|t| t.strip_prefix('v').unwrap_or(t))
            .find(|t| t.starts_with(|c: char| c.is_ascii_digit()))?;
        let mut parts = token.split('.');
        let number = |part: Option<&str>| -> Option<u32> {
            let digits: String = part?.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        };
        let major = number(parts.next())?;
        // `8.4` and `8.4.3` have a minor; `8` and `8-alpine` do not.
        let minor = number(parts.next());
        Some((major, minor))
    }
}

/// What a compose image is, as far as devflow's services go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// The Docker official image (`redis`, `valkey/valkey`, `mysql`).
    Official,
    /// Bitnami's image: another entrypoint and data layout.
    Bitnami,
    /// A repository devflow does not know that is named for the engine, or
    /// whose tag is.
    Unofficial,
    /// Ships modules (RediSearch, JSON, …) a plain server does not have.
    Modules,
    /// Redis Cluster or Sentinel.
    Topology,
    /// Percona Server or Oracle's legacy `mysql/mysql-server`.
    OtherMysql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Cache(Branch),
    Mysql,
}

/// Recognise `image`. `None` for anything devflow has no service for.
fn classify(image: &ImageRef) -> Option<(Family, Flavor)> {
    use Flavor::*;
    const V: Branch = Branch::Valkey;
    const R: Branch = Branch::Redis;
    if image.is(&["redis"]) {
        return Some((Family::Cache(R), Official));
    }
    if image.is(&["valkey", "valkey"]) || image.is(&["valkey"]) {
        return Some((Family::Cache(V), Official));
    }
    if image.is(&["mysql"]) {
        return Some((Family::Mysql, Official));
    }
    if image.bitnami("redis") {
        return Some((Family::Cache(R), Bitnami));
    }
    if image.bitnami("valkey") {
        return Some((Family::Cache(V), Bitnami));
    }
    if image.bitnami("mysql") {
        return Some((Family::Mysql, Bitnami));
    }
    for name in ["redis-cluster", "redis-sentinel"] {
        if image.bitnami(name) || image.name() == name {
            return Some((Family::Cache(R), Topology));
        }
    }
    for name in ["valkey-cluster", "valkey-sentinel"] {
        if image.bitnami(name) || image.name() == name {
            return Some((Family::Cache(V), Topology));
        }
    }
    if image.is(&["redis", "redis-stack"])
        || image.is(&["redis", "redis-stack-server"])
        || image.name() == "redis-stack"
        || image.name() == "redis-stack-server"
    {
        return Some((Family::Cache(R), Modules));
    }
    if image.name() == "valkey-bundle" {
        return Some((Family::Cache(V), Modules));
    }
    if image.is(&["mysql", "mysql-server"])
        || image.name() == "mysql-server"
        || image.name() == "percona-server"
        || image.is(&["percona"])
    {
        return Some((Family::Mysql, OtherMysql));
    }
    match image.name() {
        "redis" => return Some((Family::Cache(R), Unofficial)),
        "valkey" => return Some((Family::Cache(V), Unofficial)),
        "mysql" => return Some((Family::Mysql, Unofficial)),
        _ => {}
    }
    // An image of an unknown name whose tag says what it is
    // (`registry.example/acme/cache:valkey-8`).
    let first_word = image
        .tag
        .as_deref()
        .and_then(|t| t.split(['-', '_', '.']).next());
    match first_word {
        Some("valkey") => Some((Family::Cache(V), Unofficial)),
        Some("redis") => Some((Family::Cache(R), Unofficial)),
        _ => None,
    }
}

/// What the planner decides about one compose service.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Propose {
        /// The image the item sets.
        image: String,
        /// Why it is not the compose image, when it is not.
        image_note: Option<String>,
        confidence: Confidence,
    },
    /// devflow could branch it; a person has to decide.
    NotAutomatic(String),
    /// Outside what devflow's profile runs.
    Unsupported(String),
}

fn decide(service: &ComposeService, family: Family, flavor: Flavor) -> Outcome {
    let written = service.image.as_deref().unwrap_or("").trim();
    let image = ImageRef::parse(written);
    if !service.profiles.is_empty() {
        return Outcome::NotAutomatic(format!(
            "it is behind compose profiles ({}), so the project may not always need it",
            service.profiles.join(", ")
        ));
    }
    match family {
        Family::Cache(branch) => decide_cache(branch, written, &image, flavor),
        Family::Mysql => decide_mysql(written, &image, flavor),
    }
}

/// The compose image can be written into `devflow.toml` as it is.
fn usable_as_written(written: &str) -> bool {
    !written.is_empty()
        && !written.starts_with('-')
        && !written.contains('$')
        && !written.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn default_cache_image(branch: Branch) -> &'static str {
    match branch {
        Branch::Valkey => VALKEY_IMAGE,
        _ => REDIS_IMAGE,
    }
}

fn server_name(branch: Branch) -> &'static str {
    match branch {
        Branch::Valkey => "Valkey",
        _ => "Redis",
    }
}

fn decide_cache(branch: Branch, written: &str, image: &ImageRef, flavor: Flavor) -> Outcome {
    let name = server_name(branch);
    let default = default_cache_image(branch);
    match flavor {
        Flavor::Modules => Outcome::NotAutomatic(format!(
            "`{written}` ships modules (search, JSON, …) that devflow's plain {name} server \
             does not have, so the application could break on the branch"
        )),
        Flavor::Topology => Outcome::Unsupported(format!(
            "`{written}` is a cluster or sentinel topology; devflow branches one standalone \
             {name} server"
        )),
        Flavor::Bitnami => Outcome::Propose {
            image: default.to_string(),
            image_note: Some(format!(
                "`{written}` is Bitnami's image, which starts through its own scripts and keeps \
                 data elsewhere than /data; devflow runs `{default}` instead."
            )),
            confidence: Confidence::High,
        },
        Flavor::Unofficial | Flavor::OtherMysql => Outcome::Propose {
            image: default.to_string(),
            image_note: Some(format!(
                "`{written}` is not an image devflow knows how to run; devflow runs `{default}` \
                 instead. Set `image` if yours is a plain {name} server."
            )),
            confidence: Confidence::Medium,
        },
        Flavor::Official => {
            if !usable_as_written(written) {
                return Outcome::Propose {
                    image: default.to_string(),
                    image_note: Some(format!(
                        "the compose image (`{written}`) comes from a variable or is not an \
                         image reference devflow can write; devflow runs `{default}`."
                    )),
                    confidence: Confidence::High,
                };
            }
            match image.version() {
                Some((major, _)) if major < 7 => Outcome::Propose {
                    image: default.to_string(),
                    image_note: Some(format!(
                        "compose runs {name} {major}; devflow needs 7.0 or newer (it relies on \
                         `rdb_saves`), so it runs `{default}` and existing local data is not \
                         carried over."
                    )),
                    confidence: Confidence::High,
                },
                _ => Outcome::Propose {
                    image: written.to_string(),
                    image_note: None,
                    confidence: Confidence::High,
                },
            }
        }
    }
}

fn decide_mysql(written: &str, image: &ImageRef, flavor: Flavor) -> Outcome {
    if flavor == Flavor::OtherMysql {
        return Outcome::Unsupported(format!(
            "`{written}` is Percona Server or Oracle's legacy mysql-server image; devflow's \
             profile is the official MySQL 8.4 image only"
        ));
    }
    match image.version() {
        Some((8, Some(4))) => {}
        Some((8, None)) | None => {
            return Outcome::NotAutomatic(format!(
                "the tag of `{written}` does not say it is MySQL 8.4, the only version devflow's \
                 profile runs (`mysql:8` and `latest` move); set `image = \"{MYSQL_IMAGE}\"` \
                 yourself if the application runs on 8.4"
            ));
        }
        Some((major, minor)) => {
            let version = match minor {
                Some(minor) => format!("{major}.{minor}"),
                None => major.to_string(),
            };
            return Outcome::Unsupported(format!(
                "compose runs MySQL {version}; devflow's profile is MySQL 8.4 only"
            ));
        }
    }
    match flavor {
        Flavor::Official if usable_as_written(written) => Outcome::Propose {
            image: written.to_string(),
            image_note: None,
            confidence: Confidence::Medium,
        },
        Flavor::Official => Outcome::Propose {
            image: MYSQL_IMAGE.to_string(),
            image_note: Some(format!(
                "the compose image (`{written}`) comes from a variable or is not an image \
                 reference devflow can write; devflow runs `{MYSQL_IMAGE}`."
            )),
            confidence: Confidence::Medium,
        },
        _ => Outcome::Propose {
            image: MYSQL_IMAGE.to_string(),
            image_note: Some(format!(
                "`{written}` is not the official MySQL image (Bitnami keeps its data and \
                 configuration elsewhere); devflow runs `{MYSQL_IMAGE}` instead."
            )),
            confidence: Confidence::Medium,
        },
    }
}

/// A service key for the compose service `name`: its name as a key, or the
/// engine's when that is not one, made unique among `taken`.
fn service_key(name: &str, fallback: &str, taken: &BTreeSet<String>) -> String {
    let mut base = sanitized_key(name);
    if !config::valid_service_key(&base) || SERVICE_SUBCOMMANDS.contains(&base.as_str()) {
        base = fallback.to_string();
    }
    debug_assert!(config::valid_service_key(&base));
    let mut key = base.clone();
    let mut n = 2;
    while taken.contains(&key) || !config::valid_service_key(&key) {
        let suffix = format!("-{n}");
        let keep = base.len().min(config::MAX_KEY_LEN - suffix.len());
        key = format!("{}{suffix}", &base[..keep]);
        n += 1;
    }
    key
}

/// `name` as close to a service key as its characters allow (it may still
/// not be one).
fn sanitized_key(name: &str) -> String {
    let base: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    base.trim_start_matches(|c: char| !c.is_ascii_lowercase())
        .chars()
        .take(config::MAX_KEY_LEN)
        .collect()
}

/// Pair compose services with the services the configuration already has
/// (same key first, then in order), so a project that configured its cache
/// is not offered a second one. Returns, per candidate, the key it matched.
fn match_configured(candidates: &[String], configured: &[String]) -> Vec<Option<String>> {
    let mut used = vec![false; configured.len()];
    let mut matched: Vec<Option<usize>> = vec![None; candidates.len()];
    for (i, name) in candidates.iter().enumerate() {
        let wanted = sanitized_key(name);
        if let Some(j) = configured
            .iter()
            .enumerate()
            .position(|(j, key)| !used[j] && *key == wanted)
        {
            used[j] = true;
            matched[i] = Some(j);
        }
    }
    for slot in matched.iter_mut().filter(|slot| slot.is_none()) {
        if let Some(j) = used.iter().position(|u| !u) {
            used[j] = true;
            *slot = Some(j);
        }
    }
    matched
        .into_iter()
        .map(|slot| slot.map(|j| configured[j].clone()))
        .collect()
}

/// A compose service devflow can branch.
struct Candidate<'a> {
    service: &'a ComposeService,
    family: Family,
    flavor: Flavor,
}

impl Builder<'_> {
    /// Propose `[services.<key>]` for the compose services devflow branches.
    /// Runs after the PostgreSQL service (which owns `DATABASE_URL`) and
    /// before the compose hooks (which start what is left).
    pub fn containers(&mut self) {
        let facts = self.facts;
        let Some(compose) = &facts.compose else {
            return;
        };
        let file = compose.file.clone();
        let mut cache = Vec::new();
        let mut mysql = Vec::new();
        for service in &compose.services {
            if service.build || !valid_service_name(&service.name) {
                continue;
            }
            let Some(written) = service.image.as_deref() else {
                continue;
            };
            let Some((family, flavor)) = classify(&ImageRef::parse(written)) else {
                continue;
            };
            let candidate = Candidate {
                service,
                family,
                flavor,
            };
            match family {
                Family::Cache(_) => cache.push(candidate),
                Family::Mysql => mysql.push(candidate),
            }
        }
        if cache.is_empty() && mysql.is_empty() {
            return;
        }
        let skip: BTreeSet<&str> = cache
            .iter()
            .chain(&mysql)
            .map(|c| c.service.name.as_str())
            .collect();
        let observed = self.observed_variables(&file, &skip);
        self.cache_services(&file, &cache, &observed);
        self.mysql_service(&file, &mysql, &observed);
    }

    /// Variable names the application reads, with where they were seen: the
    /// `.env*` files (names only) and the `environment` of compose services
    /// that are not themselves candidates.
    fn observed_variables(&self, file: &str, skip: &BTreeSet<&str>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for key in &self.facts.env.keys {
            if matches!(
                key.category,
                EnvCategory::Redis
                    | EnvCategory::Mysql
                    | EnvCategory::DatabaseUrl
                    | EnvCategory::Database
            ) {
                for env_file in &key.files {
                    out.push((key.name.clone(), format!("{env_file}:{}", key.name)));
                }
            }
        }
        if let Some(compose) = &self.facts.compose {
            for service in &compose.services {
                if skip.contains(service.name.as_str()) {
                    continue;
                }
                for name in &service.environment_keys {
                    out.push((
                        name.clone(),
                        format!("{file}:services.{}.environment.{name}", service.name),
                    ));
                }
            }
        }
        out
    }

    /// The service keys and variables already spoken for.
    fn taken_keys(&self) -> BTreeSet<String> {
        let mut taken: BTreeSet<String> = self
            .cfg
            .iter()
            .flat_map(|cfg| cfg.services.keys().cloned())
            .collect();
        taken.extend(
            self.items
                .iter()
                .filter_map(|item| item.id.strip_prefix("service:").map(str::to_string)),
        );
        taken
    }

    fn taken_variables(&self) -> BTreeSet<String> {
        let mut taken = BTreeSet::new();
        if let Some(cfg) = &self.cfg {
            taken.extend(cfg.exported_variables().into_iter().map(|(name, _)| name));
            taken.extend(cfg.env.vars.keys().cloned());
        }
        if self.has_pg {
            taken.insert("DATABASE_URL".to_string());
        }
        for item in &self.items {
            if item.kind == ItemKind::Service
                && let Some(name) = item.value.get("env").and_then(|v| v.as_str())
            {
                taken.insert(name.to_string());
            }
        }
        taken
    }

    fn configured(&self, family: fn(Branch) -> bool) -> Vec<String> {
        self.cfg
            .iter()
            .flat_map(|cfg| cfg.containers())
            .filter(|(_, engine, _)| family(*engine))
            .map(|(key, _, _)| key.to_string())
            .collect()
    }

    fn cache_services(
        &mut self,
        file: &str,
        candidates: &[Candidate<'_>],
        observed: &[(String, String)],
    ) {
        if candidates.is_empty() {
            return;
        }
        let names: Vec<String> = candidates.iter().map(|c| c.service.name.clone()).collect();
        let configured = self.configured(|e| matches!(e, Branch::Valkey | Branch::Redis));
        let matches = match_configured(&names, &configured);
        for (candidate, matched) in candidates.iter().zip(matches) {
            let service = candidate.service;
            let written = service.image.as_deref().unwrap_or("");
            if let Some(key) = matched {
                self.branched_compose.insert(service.name.clone());
                self.warn(format!(
                    "{file}: the compose service `{}` ({written}) is already configured as \
                     services.{key}; the proposed compose hooks do not start it",
                    service.name
                ));
                continue;
            }
            let Family::Cache(branch) = candidate.family else {
                continue;
            };
            match decide(service, candidate.family, candidate.flavor) {
                Outcome::NotAutomatic(reason) => self.not_branched(
                    file,
                    service,
                    "not proposed automatically",
                    &reason,
                    README_CACHE,
                ),
                Outcome::Unsupported(reason) => {
                    self.not_branched(file, service, "unsupported", &reason, README_CACHE)
                }
                Outcome::Propose {
                    image,
                    image_note,
                    confidence,
                } => self.propose_cache(
                    file, candidate, branch, image, image_note, confidence, observed,
                ),
            }
        }
    }

    /// Record (and say) that devflow leaves `service` to the compose hooks.
    fn not_branched(
        &mut self,
        file: &str,
        service: &ComposeService,
        kind: &str,
        reason: &str,
        readme: &str,
    ) {
        let written = service.image.as_deref().unwrap_or("");
        let pointer = if kind == "unsupported" {
            format!("See {readme} for what devflow supports")
        } else {
            format!("To branch it anyway, configure it by hand: see {readme}")
        };
        self.warn(format!(
            "{file}: compose service `{}` ({written}): {kind}: {reason}. {pointer}. The proposed \
             compose hooks start it instead.",
            service.name
        ));
        self.unbranched_compose.insert(
            service.name.clone(),
            format!("{kind}: {reason} (see {readme})"),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn propose_cache(
        &mut self,
        file: &str,
        candidate: &Candidate<'_>,
        branch: Branch,
        image: String,
        image_note: Option<String>,
        confidence: Confidence,
        observed: &[(String, String)],
    ) {
        let service = candidate.service;
        let name = server_name(branch);
        let engine = branch.as_str();
        let key = service_key(&service.name, engine, &self.taken_keys());
        let mut taken = self.taken_variables();
        let seen: Vec<&str> = observed.iter().map(|(n, _)| n.as_str()).collect();

        // The variable: what the application reads, else the default.
        let usable = |name: &str| {
            config::valid_env_name(name)
                && !crate::env_file::is_reserved(name)
                && !taken.contains(name)
        };
        let mut risks = Vec::new();
        let variable = CACHE_URLS
            .iter()
            .copied()
            .find(|n| seen.contains(n) && usable(n))
            .map(str::to_string)
            .or_else(|| {
                CACHE_URLS.first().copied().filter(|n| usable(n)).map(|n| {
                    if !seen.iter().any(|s| CACHE_URLS.contains(s)) {
                        risks.push(format!(
                            "No .env file or compose service names the cache URL, so devflow \
                                 exports it as {n}; check the application reads that, or set \
                                 `env` on the service."
                        ));
                    }
                    n.to_string()
                })
            })
            .or_else(|| {
                let own = format!("{}_URL", key.to_ascii_uppercase().replace('-', "_"));
                usable(&own).then(|| {
                    risks.push(format!(
                        "Another service already exports the usual cache variables, so this \
                         one exports its URL as {own}: point the application at it (or set \
                         `env`)."
                    ));
                    own
                })
            });
        let Some(variable) = variable else {
            self.not_branched(
                file,
                service,
                "not proposed automatically",
                "no free variable name is left for its URL; set `env` on the service by hand",
                README_CACHE,
            );
            return;
        };
        taken.insert(variable.clone());
        let explicit = variable != config::DEFAULT_CACHE_ENV;

        // Risks devflow knows about.
        if let Some(note) = image_note {
            risks.insert(0, note);
        }
        let config_mounted = service.volumes.iter().any(|v| {
            let target = v.target.to_ascii_lowercase();
            target.ends_with(".conf")
                || target.contains("/etc/redis")
                || target.contains("/etc/valkey")
        });
        let aof_env = service
            .environment_keys
            .iter()
            .any(|k| k.to_ascii_uppercase().contains("AOF"));
        risks.push(if config_mounted || aof_env {
            format!(
                "The compose service configures {name} itself ({}); devflow runs it with AOF off \
                 and RDB snapshots (`--save` policy, data in /data), so persistence, memory and \
                 ACL settings do not carry over, and a source with AOF on is refused.",
                if config_mounted {
                    "a mounted configuration file"
                } else {
                    "an AOF setting in its environment"
                }
            )
        } else {
            format!(
                "devflow runs {name} with AOF off and RDB snapshots (`--save` policy, data in \
                 /data); a persistence, memory or ACL setting in compose (a `command`, a mounted \
                 redis.conf) does not carry over, and a source with AOF on is refused."
            )
        });
        if !service.volumes.is_empty() {
            risks.push(
                "The compose service's volume is not carried over: a new workspace starts empty \
                 (`--data-from empty`) or from another workspace's data."
                    .to_string(),
            );
        }
        risks.push(format!(
            "While this item is accepted the proposed compose hooks do not start `{}`; decline \
             it (`--reject service:{key}`) and they start it instead. The compose service can \
             stay in {file} for other uses, but the application must use the URL devflow exports \
             as {variable}, not the compose container's.",
            service.name
        ));
        self.cloned_env_risk(&variable, &mut risks);

        let mut evidence = service.evidence.clone();
        evidence.extend(self.observed_evidence(observed, &[&variable]));
        evidence.extend(self.driver_evidence(crate::detect::Engine::Redis));

        let key_path = |part: &str| vec!["services".to_string(), key.clone(), part.to_string()];
        let mut ops = vec![PatchOp::Table {
            path: vec!["services".into(), key.clone()],
            comment: Some(format!(
                "{name} branches: every workspace gets its own container, cloned from its \
                 source's data (copy-on-write where the filesystem allows)."
            )),
            dotted: false,
        }];
        let set = |part: &str, value: &str| PatchOp::Set {
            path: key_path(part),
            value: json!(value),
            comment: None,
        };
        ops.push(set("engine", engine));
        ops.push(set("image", &image));
        if explicit {
            ops.push(set("env", &variable));
        }
        let id = format!("service:{key}");
        let item_id = id.clone();
        let spec = Spec::new(
            id,
            ItemKind::Service,
            self.devflow_file(),
            format!("Branch {name} per workspace with a devflow-managed container ({image})"),
        )
        .value(json!({
            "key": key,
            "engine": engine,
            "image": image,
            "env": variable,
            "compose_service": service.name,
        }))
        .confidence(confidence)
        .evidence(evidence)
        .risks(risks)
        .ops(ops);
        self.branch_or_compose(spec, &service.name, item_id);
    }

    /// Push a service item. Added to the plan, it is devflow's container when
    /// accepted and the compose start when not (decided at selection: see
    /// `compose`); already in the configuration, it is devflow's.
    fn branch_or_compose(&mut self, spec: Spec, compose_service: &str, item_id: String) {
        match self.push(spec) {
            Pushed::Item => {
                self.branched_compose.insert(compose_service.to_string());
                self.branch_fallback
                    .insert(compose_service.to_string(), item_id);
            }
            Pushed::Satisfied => {
                self.branched_compose.insert(compose_service.to_string());
            }
            Pushed::Blocked | Pushed::Unusable => {}
        }
    }

    fn mysql_service(
        &mut self,
        file: &str,
        candidates: &[Candidate<'_>],
        observed: &[(String, String)],
    ) {
        if candidates.is_empty() {
            return;
        }
        let names: Vec<String> = candidates.iter().map(|c| c.service.name.clone()).collect();
        let configured = self.configured(|e| e == Branch::Mysql);
        // At most one mysql service: the first candidate is the one, and a
        // service the configuration has already is that one.
        let matches = match_configured(&names, &configured);
        let mut chosen: Option<String> = configured.first().cloned();
        for (candidate, matched) in candidates.iter().zip(matches) {
            let service = candidate.service;
            let written = service.image.as_deref().unwrap_or("");
            if let Some(key) = matched {
                self.branched_compose.insert(service.name.clone());
                self.warn(format!(
                    "{file}: the compose service `{}` ({written}) is already configured as \
                     services.{key}; the proposed compose hooks do not start it",
                    service.name
                ));
                continue;
            }
            match decide(service, candidate.family, candidate.flavor) {
                Outcome::NotAutomatic(reason) => self.not_branched(
                    file,
                    service,
                    "not proposed automatically",
                    &reason,
                    README_MYSQL,
                ),
                Outcome::Unsupported(reason) => {
                    self.not_branched(file, service, "unsupported", &reason, README_MYSQL)
                }
                // A second service that would be proposed: only one is.
                Outcome::Propose { .. } if chosen.is_some() => self.not_branched(
                    file,
                    service,
                    "not proposed automatically",
                    &format!(
                        "devflow supports at most one mysql service per project for now, and \
                         `{}` is the one",
                        chosen.as_deref().unwrap_or_default()
                    ),
                    README_MYSQL,
                ),
                Outcome::Propose {
                    image,
                    image_note,
                    confidence,
                } => {
                    let key = service_key("mysql", "mysql", &self.taken_keys());
                    self.propose_mysql(
                        file, service, &key, image, image_note, confidence, observed,
                    );
                    if self.branched_compose.contains(&service.name) {
                        chosen = Some(service.name.clone());
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn propose_mysql(
        &mut self,
        file: &str,
        service: &ComposeService,
        key: &str,
        image: String,
        image_note: Option<String>,
        confidence: Confidence,
        observed: &[(String, String)],
    ) {
        // `DATABASE_URL`, or `MYSQL_URL` beside a PostgreSQL service: core's
        // default, so no `env` is written.
        let variable = if self.has_pg {
            MYSQL_URL_ENV
        } else {
            config::DEFAULT_MYSQL_ENV
        };
        let seen: Vec<&str> = observed.iter().map(|(n, _)| n.as_str()).collect();
        let mut risks = vec![
            "Preview: cloning a MySQL source interrupts it for a few seconds, and the engine has \
             not been through an independent review yet."
                .to_string(),
        ];
        if let Some(note) = image_note {
            risks.push(note);
        }
        if !seen.iter().any(|s| MYSQL_URLS.contains(s)) {
            risks.push(format!(
                "No .env file or compose service names the database URL, so devflow exports it \
                 as {variable}; check the application reads that, or set `env` on the service."
            ));
        } else if !seen.contains(&variable) {
            risks.push(format!(
                "devflow exports the MySQL URL as {variable}, but the application's files name \
                 {}; set `env` on the service if it reads that one.",
                MYSQL_URLS
                    .iter()
                    .filter(|n| seen.contains(n))
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" and ")
            ));
        }
        risks.push(
            "devflow exports the URL together with MYSQL_HOST, MYSQL_PORT, MYSQL_USER, \
             MYSQL_PASSWORD and MYSQL_DATABASE (the database is named after the project unless \
             `database` is set)."
                .to_string(),
        );
        risks.push(
            "devflow's MySQL is InnoDB only (MyISAM and MEMORY tables are refused), has no binary \
             log and runs no init scripts."
                .to_string(),
        );
        if !service.init_scripts.is_empty() {
            risks.push(format!(
                "compose service `{}` mounts init scripts ({}); devflow's MySQL does not run \
                 them. Move schema setup into migrations or a setup hook.",
                service.name,
                service.init_scripts.join(", ")
            ));
        }
        if !service.volumes.is_empty() {
            risks.push(
                "The compose service's volume is not carried over: a new workspace starts empty \
                 (`--data-from empty`) or from another workspace's data."
                    .to_string(),
            );
        }
        risks.push(format!(
            "At most one mysql service is supported for now. While this item is accepted the \
             proposed compose hooks do not start `{}`; decline it (`--reject service:{key}`) and \
             they start it instead. It can stay in {file} for other uses, but the application \
             must use the URL devflow exports as {variable}, not the compose container's.",
            service.name
        ));
        self.cloned_env_risk(variable, &mut risks);

        let mut evidence = service.evidence.clone();
        evidence.extend(self.observed_evidence(observed, &MYSQL_URLS));
        evidence.extend(self.driver_evidence(crate::detect::Engine::Mysql));

        let path = |part: Option<&str>| {
            let mut path = vec!["services".to_string(), key.to_string()];
            path.extend(part.map(str::to_string));
            path
        };
        let set = |part: &str, value: &str| PatchOp::Set {
            path: path(Some(part)),
            value: json!(value),
            comment: None,
        };
        let ops = vec![
            PatchOp::Table {
                path: path(None),
                comment: Some(
                    "MySQL 8.4 branches (preview): every workspace gets its own server, cloned \
                     from its source's data; cloning interrupts the source for a few seconds."
                        .to_string(),
                ),
                dotted: false,
            },
            set("engine", "mysql"),
            set("image", &image),
        ];
        let item_id = format!("service:{key}");
        let spec = Spec::new(
            item_id.clone(),
            ItemKind::Service,
            self.devflow_file(),
            format!(
                "Branch MySQL 8.4 per workspace with a devflow-managed container ({image}) \
                 (preview)"
            ),
        )
        .value(json!({
            "key": key,
            "engine": "mysql",
            "image": image,
            "env": variable,
            "compose_service": service.name,
            "preview": true,
        }))
        .confidence(confidence)
        .evidence(evidence)
        .risks(risks)
        .ops(ops);
        self.branch_or_compose(spec, &service.name, item_id);
    }

    /// A cloned `.env` that defines `variable` keeps the original value.
    fn cloned_env_risk(&self, variable: &str, risks: &mut Vec<String>) {
        let copied: Vec<&str> = self
            .items
            .iter()
            .filter(|item| item.kind == ItemKind::Copy)
            .filter_map(|item| item.value.get("path").and_then(|p| p.as_str()))
            .collect();
        let defined = self.facts.env.keys.iter().any(|key| {
            key.name == variable && key.files.iter().any(|f| copied.contains(&f.as_str()))
        });
        if defined {
            risks.push(format!(
                "A cloned .env also defines {variable}. Most dotenv loaders keep a variable that \
                 is already in the process environment, so load .devflow/env into the process \
                 (see mise:env-file) or the application keeps using the original server."
            ));
        }
    }

    /// `file:NAME` evidence for the variables in `names` (at most four).
    fn observed_evidence(&self, observed: &[(String, String)], names: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (name, evidence) in observed {
            if names.contains(&name.as_str()) && !out.contains(evidence) {
                out.push(evidence.clone());
            }
        }
        out.truncate(4);
        out
    }

    fn driver_evidence(&self, engine: crate::detect::Engine) -> Vec<String> {
        let mut out: Vec<String> = self
            .facts
            .drivers
            .iter()
            .filter(|d| d.engine == engine)
            .flat_map(|d| d.evidence.clone())
            .collect();
        out.truncate(2);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(image: &str) -> Option<(Family, Flavor)> {
        classify(&ImageRef::parse(image))
    }

    #[test]
    fn registries_library_and_digests_are_not_part_of_the_name() {
        for image in [
            "redis",
            "redis:7-alpine",
            "docker.io/library/redis:7",
            "library/redis",
            "registry-1.docker.io/library/redis:7.2",
            "REDIS:7@sha256:abc",
        ] {
            assert_eq!(
                kind(image),
                Some((Family::Cache(Branch::Redis), Flavor::Official)),
                "{image}"
            );
        }
        for image in [
            "valkey/valkey:8-alpine",
            "docker.io/valkey/valkey:8",
            "valkey",
            "valkey:8",
        ] {
            assert_eq!(
                kind(image),
                Some((Family::Cache(Branch::Valkey), Flavor::Official)),
                "{image}"
            );
        }
        for image in ["mysql:8.4", "docker.io/library/mysql:8.4.3", "mysql"] {
            assert_eq!(
                kind(image),
                Some((Family::Mysql, Flavor::Official)),
                "{image}"
            );
        }
    }

    #[test]
    fn other_vendors_and_lookalikes_are_told_apart() {
        use Flavor::*;
        assert_eq!(
            kind("bitnami/redis:7.2"),
            Some((Family::Cache(Branch::Redis), Bitnami))
        );
        assert_eq!(
            kind("bitnami/valkey"),
            Some((Family::Cache(Branch::Valkey), Bitnami))
        );
        assert_eq!(kind("bitnami/mysql:8.4"), Some((Family::Mysql, Bitnami)));
        assert_eq!(
            kind("redis/redis-stack:latest"),
            Some((Family::Cache(Branch::Redis), Modules))
        );
        assert_eq!(
            kind("valkey/valkey-bundle"),
            Some((Family::Cache(Branch::Valkey), Modules))
        );
        assert_eq!(
            kind("bitnami/redis-cluster:7"),
            Some((Family::Cache(Branch::Redis), Topology))
        );
        assert_eq!(
            kind("percona/percona-server:8.4"),
            Some((Family::Mysql, OtherMysql))
        );
        assert_eq!(
            kind("mysql/mysql-server:8.0"),
            Some((Family::Mysql, OtherMysql))
        );
        // A name or a tag that says what it is, from nobody devflow knows.
        assert_eq!(
            kind("ghcr.io/acme/redis:7"),
            Some((Family::Cache(Branch::Redis), Unofficial))
        );
        assert_eq!(
            kind("registry.example/acme/cache:valkey-8"),
            Some((Family::Cache(Branch::Valkey), Unofficial))
        );
        assert_eq!(
            kind("acme/store:redis-7-alpine"),
            Some((Family::Cache(Branch::Redis), Unofficial))
        );
        for image in [
            "postgres:16",
            "mariadb:11",
            "memcached",
            "acme/mailer",
            "redisinsight",
        ] {
            assert_eq!(kind(image), None, "{image}");
        }
    }

    #[test]
    fn versions_come_from_the_first_numeric_token_of_the_tag() {
        let v = |image: &str| ImageRef::parse(image).version();
        assert_eq!(v("redis:7.2-alpine"), Some((7, Some(2))));
        assert_eq!(v("redis:8-alpine"), Some((8, None)));
        assert_eq!(v("mysql:8.4.3-oraclelinux9"), Some((8, Some(4))));
        assert_eq!(v("mysql:8.40"), Some((8, Some(40))));
        assert_eq!(v("redis:v6.2"), Some((6, Some(2))));
        assert_eq!(v("redis:alpine"), None);
        assert_eq!(v("redis"), None);
        assert_eq!(v("mysql:latest"), None);
    }

    fn service(image: &str) -> ComposeService {
        ComposeService {
            name: "cache".into(),
            image: Some(image.into()),
            build: false,
            engine: crate::detect::Engine::Redis,
            flavor: None,
            tag: None,
            major_version: None,
            ports: Vec::new(),
            container_name: None,
            volumes: Vec::new(),
            init_scripts: Vec::new(),
            environment_keys: Vec::new(),
            profiles: Vec::new(),
            evidence: Vec::new(),
        }
    }

    fn decided(image: &str) -> Outcome {
        let (family, flavor) = kind(image).expect("recognised");
        decide(&service(image), family, flavor)
    }

    #[test]
    fn cache_images_keep_the_compose_image_when_devflow_can_run_it() {
        for image in [
            "redis:7",
            "redis:7.2-alpine",
            "docker.io/library/redis:8",
            "valkey/valkey:8",
            "redis",
            "redis:alpine",
        ] {
            match decided(image) {
                Outcome::Propose {
                    image: chosen,
                    image_note: None,
                    confidence: Confidence::High,
                } => assert_eq!(chosen, image),
                other => panic!("{image}: {other:?}"),
            }
        }
    }

    #[test]
    fn cache_images_devflow_cannot_run_get_its_default_and_the_reason() {
        for (image, default) in [
            ("redis:6.2", REDIS_IMAGE),
            ("bitnami/redis:7.2", REDIS_IMAGE),
            ("bitnami/valkey:8", VALKEY_IMAGE),
        ] {
            match decided(image) {
                Outcome::Propose {
                    image: chosen,
                    image_note: Some(note),
                    ..
                } => {
                    assert_eq!(chosen, default, "{image}");
                    assert!(note.contains(default), "{note}");
                }
                other => panic!("{image}: {other:?}"),
            }
        }
        match decided("ghcr.io/acme/redis:7") {
            Outcome::Propose {
                confidence: Confidence::Medium,
                image_note: Some(_),
                ..
            } => {}
            other => panic!("{other:?}"),
        }
        // A variable the compose file resolves at run time cannot be written.
        match decided("redis:${REDIS_TAG}") {
            Outcome::Propose {
                image,
                image_note: Some(_),
                ..
            } => assert_eq!(image, REDIS_IMAGE),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn modules_topologies_and_profiles_are_not_proposed() {
        assert!(matches!(
            decided("redis/redis-stack"),
            Outcome::NotAutomatic(_)
        ));
        assert!(matches!(
            decided("valkey/valkey-bundle"),
            Outcome::NotAutomatic(_)
        ));
        assert!(matches!(
            decided("bitnami/redis-cluster"),
            Outcome::Unsupported(_)
        ));
        let mut profiled = service("redis:7");
        profiled.profiles = vec!["extras".into()];
        assert!(matches!(
            decide(&profiled, Family::Cache(Branch::Redis), Flavor::Official),
            Outcome::NotAutomatic(reason) if reason.contains("extras")
        ));
    }

    #[test]
    fn only_mysql_8_4_is_proposed() {
        match decided("mysql:8.4") {
            Outcome::Propose {
                image,
                image_note: None,
                confidence: Confidence::Medium,
            } => {
                assert_eq!(image, "mysql:8.4")
            }
            other => panic!("{other:?}"),
        }
        match decided("docker.io/library/mysql:8.4.3-oraclelinux9") {
            Outcome::Propose {
                image_note: None, ..
            } => {}
            other => panic!("{other:?}"),
        }
        match decided("bitnami/mysql:8.4") {
            Outcome::Propose {
                image,
                image_note: Some(_),
                ..
            } => assert_eq!(image, MYSQL_IMAGE),
            other => panic!("{other:?}"),
        }
        for image in ["mysql:8.0", "mysql:5.7", "mysql:9.1", "mysql:8.40"] {
            assert!(matches!(decided(image), Outcome::Unsupported(_)), "{image}");
        }
        for image in [
            "mysql",
            "mysql:latest",
            "mysql:8",
            "mysql:lts",
            "mysql:8-oracle",
        ] {
            assert!(
                matches!(decided(image), Outcome::NotAutomatic(_)),
                "{image}"
            );
        }
        assert!(matches!(
            decided("percona/percona-server:8.4"),
            Outcome::Unsupported(_)
        ));
    }

    #[test]
    fn keys_come_from_the_compose_name_and_stay_unique() {
        let none = BTreeSet::new();
        assert_eq!(service_key("cache", "redis", &none), "cache");
        assert_eq!(service_key("Redis_Cache", "redis", &none), "redis_cache");
        assert_eq!(service_key("1cache", "redis", &none), "cache");
        // A word `devflow service` reads as a subcommand, or nothing usable.
        assert_eq!(service_key("up", "valkey", &none), "valkey");
        assert_eq!(service_key("###", "valkey", &none), "valkey");
        let taken: BTreeSet<String> = ["cache".to_string(), "cache-2".to_string()].into();
        assert_eq!(service_key("cache", "redis", &taken), "cache-3");
        let long = "a".repeat(40);
        assert!(config::valid_service_key(&service_key(
            &long, "redis", &none
        )));
    }

    #[test]
    fn configured_services_are_matched_by_key_then_in_order() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            match_configured(&names(&["cache", "queue"]), &names(&["queue"])),
            vec![None, Some("queue".to_string())]
        );
        assert_eq!(
            match_configured(&names(&["redis"]), &names(&["sessions"])),
            vec![Some("sessions".to_string())]
        );
        assert_eq!(
            match_configured(&names(&["a", "b"]), &names(&["x"])),
            vec![Some("x".to_string()), None]
        );
        assert_eq!(match_configured(&names(&["a"]), &[]), vec![None]);
    }
}
