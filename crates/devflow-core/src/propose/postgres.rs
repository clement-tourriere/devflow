//! The PostgreSQL service: when one is worth proposing, and which image.
use super::plan::{Builder, PostgresChoice, Pushed, Spec};
use super::{Confidence, ItemKind, PatchOp};
use crate::config::PostgresMode;
use crate::detect::{ComposeService, Engine, EnvCategory};
use serde_json::json;

/// The key a proposed PostgreSQL service gets.
pub(super) const SERVICE_KEY: &str = "db";
/// What devflow's managed server runs unless the compose image says better.
const DEFAULT_IMAGE: &str = "postgres:18";

impl Builder<'_> {
    pub fn postgres(&mut self) {
        let file = self.devflow_file();
        let choice = self.options.postgres;

        // A service the configuration already has.
        let present = self.cfg.as_ref().and_then(|cfg| {
            cfg.postgres()
                .map(|(key, section)| (key.to_string(), section.mode))
        });
        if let Some((key, mode)) = present {
            self.has_pg = true;
            match (choice, mode) {
                (Some(PostgresChoice::Managed), PostgresMode::External)
                | (Some(PostgresChoice::External), PostgresMode::Managed) => {
                    let wanted = if choice == Some(PostgresChoice::Managed) {
                        "managed"
                    } else {
                        "external"
                    };
                    let have = if mode == PostgresMode::Managed {
                        "managed"
                    } else {
                        "external"
                    };
                    let spec = Spec::new(
                        format!("service:{key}"),
                        ItemKind::Service,
                        file,
                        format!("Use a {wanted} PostgreSQL server for services.{key}"),
                    )
                    .value(json!({"key": key, "engine": "postgres", "mode": wanted}))
                    .evidence([format!("{file}:services.{key}.mode")]);
                    self.report_conflict(
                        spec,
                        format!(
                            "services.{key} is already {have}; change its mode by hand if you \
                             want {wanted}"
                        ),
                    );
                }
                (Some(PostgresChoice::None), _) => {
                    self.warn(format!(
                        "--postgres none ignored: {file} already configures services.{key}"
                    ));
                    self.satisfied.push(format!("service:{key}"));
                }
                _ => self.satisfied.push(format!("service:{key}")),
            }
            return;
        }
        if choice == Some(PostgresChoice::None) {
            return;
        }

        let facts = self.facts;
        let compose: Vec<&ComposeService> = facts.compose_services(Engine::Postgres);
        let migration_pg: Vec<_> = facts
            .migrations
            .iter()
            .filter(|m| m.database == Some(Engine::Postgres))
            .collect();
        let drivers: Vec<_> = facts
            .drivers
            .iter()
            .filter(|d| d.engine == Engine::Postgres)
            .collect();
        let env_keys: Vec<_> = facts
            .env
            .keys
            .iter()
            .filter(|k| {
                matches!(
                    k.category,
                    EnvCategory::DatabaseUrl | EnvCategory::Postgres | EnvCategory::Database
                )
            })
            .collect();

        let mut evidence = Vec::new();
        for service in &compose {
            evidence.extend(service.evidence.clone());
        }
        for fact in &migration_pg {
            evidence.extend(fact.evidence.clone());
        }
        for driver in &drivers {
            evidence.extend(driver.evidence.clone());
        }
        let mut env_evidence: Vec<String> = env_keys
            .iter()
            .flat_map(|k| k.files.iter().map(move |f| format!("{f}:{}", k.name)))
            .collect();
        env_evidence.truncate(6);
        evidence.extend(env_evidence.clone());

        let (mode, mut confidence) = match choice {
            Some(PostgresChoice::External) => (PostgresMode::External, Confidence::High),
            Some(PostgresChoice::Managed) => (PostgresMode::Managed, Confidence::High),
            _ => {
                let confidence = if !compose.is_empty()
                    || !migration_pg.is_empty()
                    || (!drivers.is_empty() && !env_keys.is_empty())
                {
                    Confidence::High
                } else if !drivers.is_empty() {
                    Confidence::Medium
                } else {
                    // Variables alone do not say which engine.
                    return;
                };
                (PostgresMode::Managed, confidence)
            }
        };
        if choice.is_some() {
            evidence.insert(
                0,
                match mode {
                    PostgresMode::Managed => "--postgres managed".to_string(),
                    PostgresMode::External => "--postgres external".to_string(),
                },
            );
        }

        let mut risks = Vec::new();
        let (image, image_risks) = managed_image(&compose);
        if mode == PostgresMode::Managed {
            risks.extend(image_risks);
        }
        if compose.is_empty()
            && migration_pg.is_empty()
            && !drivers.is_empty()
            && env_keys.is_empty()
        {
            risks.push(
                "A PostgreSQL driver is a dependency, but no .env file or compose service names \
                 a database; check the application reads DATABASE_URL."
                    .to_string(),
            );
            // Thin evidence only lowers a guess: `--postgres managed|external`
            // is the person's own decision and stays a default.
            if choice.is_none() {
                confidence = Confidence::Medium;
            }
        }
        for service in &compose {
            if !service.init_scripts.is_empty() {
                risks.push(format!(
                    "compose service `{}` mounts init scripts ({}); devflow's server does not run \
                     them. Move extension and schema setup into migrations or a setup hook.",
                    service.name,
                    service.init_scripts.join(", ")
                ));
            }
        }
        // A cloned .env keeps pointing at the main database.
        let copied: Vec<String> = self
            .items
            .iter()
            .filter(|item| item.kind == ItemKind::Copy)
            .filter_map(|item| item.value.get("path").and_then(|p| p.as_str()))
            .map(str::to_string)
            .collect();
        let env_with_url = facts.env.keys.iter().any(|key| {
            key.category == EnvCategory::DatabaseUrl
                && key.files.iter().any(|file| copied.contains(file))
        });
        if env_with_url {
            risks.push(
                "A cloned .env also defines DATABASE_URL. Most dotenv loaders keep a variable that \
                 is already in the process environment, so load .devflow/env into the process \
                 (see mise:env-file) or the application keeps using the original database."
                    .to_string(),
            );
        }
        if !compose.is_empty() {
            let names: Vec<String> = compose
                .iter()
                .map(|s| {
                    format!(
                        "`{}`{}",
                        s.name,
                        s.image
                            .as_deref()
                            .map(|i| format!(" ({i})"))
                            .unwrap_or_default()
                    )
                })
                .collect();
            self.warn(format!(
                "{} keeps the compose service {}; workspaces use devflow's PostgreSQL instead, so \
                 the proposed compose hooks do not start it",
                facts
                    .compose
                    .as_ref()
                    .map_or("compose.yaml", |c| c.file.as_str()),
                names.join(", ")
            ));
        }

        let mut ops = vec![PatchOp::Table {
            path: vec!["services".into(), SERVICE_KEY.into()],
            comment: Some(match mode {
                PostgresMode::Managed => {
                    "PostgreSQL branches: every workspace gets its own database, cloned from its \
                     source (copy-on-write where the filesystem allows)."
                        .to_string()
                }
                PostgresMode::External => {
                    "PostgreSQL branches on an existing server (PostgreSQL 18+). Put the admin URL \
                     under [services.db] in devflow.local.toml or in DEVFLOW_PG_ADMIN_URL, not \
                     here."
                        .to_string()
                }
            }),
            dotted: false,
        }];
        let set = |key: &str, value: &str| PatchOp::Set {
            path: vec!["services".into(), SERVICE_KEY.into(), key.into()],
            value: json!(value),
            comment: None,
        };
        ops.push(set("engine", "postgres"));
        match mode {
            PostgresMode::Managed => {
                ops.push(set("mode", "managed"));
                ops.push(set("image", &image));
            }
            PostgresMode::External => ops.push(set("mode", "external")),
        }
        let (title, value) = match mode {
            PostgresMode::Managed => (
                format!("Branch PostgreSQL per workspace with a devflow-managed server ({image})"),
                json!({"key": SERVICE_KEY, "engine": "postgres", "mode": "managed", "image": image}),
            ),
            PostgresMode::External => (
                "Branch PostgreSQL per workspace on an existing server (PostgreSQL 18+)"
                    .to_string(),
                json!({"key": SERVICE_KEY, "engine": "postgres", "mode": "external"}),
            ),
        };
        let spec = Spec::new(
            format!("service:{SERVICE_KEY}"),
            ItemKind::Service,
            file,
            title,
        )
        .value(value)
        .confidence(confidence)
        .evidence(evidence)
        .risks(risks)
        .ops(ops);
        let id = format!("service:{SERVICE_KEY}");
        match self.push(spec) {
            Pushed::Item => {
                self.pg_item = Some(id);
                self.has_pg = true;
            }
            Pushed::Satisfied => self.has_pg = true,
            Pushed::Blocked | Pushed::Unusable => {}
        }
    }
}

/// The image devflow's managed server should run for these compose
/// PostgreSQL services, and what to watch out for.
fn managed_image(compose: &[&ComposeService]) -> (String, Vec<String>) {
    let mut risks = Vec::new();
    let Some(service) = compose.first() else {
        return (DEFAULT_IMAGE.to_string(), risks);
    };
    let image = service.image.as_deref().unwrap_or("");
    let repository = repository_of(image);
    if let Some(major) = service.major_version
        && major < 18
    {
        risks.push(format!(
            "compose runs PostgreSQL {major}; devflow's server needs 18 or newer \
             (file_copy_method = clone), so existing local data is not carried over and \
             version-specific behavior may differ."
        ));
    }
    let keep_newer = service
        .major_version
        .filter(|major| *major >= 18 && service.flavor.is_none());
    let choice = match (service.flavor.as_deref(), repository.as_str()) {
        (None, _) => match keep_newer {
            Some(major) => format!("postgres:{major}"),
            None => DEFAULT_IMAGE.to_string(),
        },
        (Some("pgvector"), "pgvector/pgvector") => "pgvector/pgvector:pg18".to_string(),
        (Some("postgis"), "postgis/postgis") => "postgis/postgis:18-3.6".to_string(),
        (Some("timescaledb"), "timescale/timescaledb") => {
            "timescale/timescaledb:latest-pg18".to_string()
        }
        (Some(flavor), _) => {
            risks.push(format!(
                "compose uses a {flavor} image ({image}) with no known PostgreSQL 18 equivalent; \
                 `postgres:18` does not ship those extensions, so pick the image yourself."
            ));
            DEFAULT_IMAGE.to_string()
        }
    };
    if choice != DEFAULT_IMAGE && service.flavor.is_some() {
        risks.push(format!(
            "`{choice}` is the usual PostgreSQL 18 image for {image}; check the tag exists and \
             that your extensions support 18."
        ));
    }
    (choice, risks)
}

/// `image` without its registry host's port, tag and digest, lowercase:
/// `ghcr.io/foo/bar:1` is `foo/bar`, `postgres:16` is `postgres`.
fn repository_of(image: &str) -> String {
    let image = image.trim().to_ascii_lowercase();
    let without_digest = image.split('@').next().unwrap_or("");
    let last_slash = without_digest.rfind('/').map_or(0, |i| i + 1);
    let repository = match without_digest[last_slash..].rfind(':') {
        Some(i) => &without_digest[..last_slash + i],
        None => without_digest,
    };
    let mut parts: Vec<&str> = repository.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() > 1
        && parts
            .first()
            .is_some_and(|p| p.contains('.') || p.contains(':') || *p == "localhost")
    {
        parts.remove(0);
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repositories_drop_registry_tag_and_digest() {
        assert_eq!(repository_of("postgres:16-alpine"), "postgres");
        assert_eq!(repository_of("ghcr.io/foo/bar:1"), "foo/bar");
        assert_eq!(
            repository_of("localhost:5000/pgvector/pgvector:pg16"),
            "pgvector/pgvector"
        );
        assert_eq!(
            repository_of("pgvector/pgvector@sha256:abc"),
            "pgvector/pgvector"
        );
    }
}
