//! Seed data for runs that cannot prompt (`[data] noninteractive_from`).
//!
//! A non-interactive creation or adoption that was not told where its data
//! comes from (`--data-from`) takes each service's data from the configured
//! snapshot. Snapshots are per service and named, so the lookup is made
//! service by service; this module decides, the caller finds the snapshot.
//!
//! A service without that snapshot **fails**, with typed remedies: its default
//! data source is a live workspace, and cloning one ends PostgreSQL sessions
//! or stops MySQL, which a run nobody watches must not do on its own. The one
//! way out in the configuration is `noninteractive_fallback = "empty"`, an
//! empty start for such a service, with a warning.
//!
//! The default workspace is never seeded: its root branch is shared (every
//! workspace without another source clones it) and stays what it always was,
//! empty, whoever happened to create it first.
use super::SetupOptions;
use crate::config::{Config, Engine, SeedFallback};
use crate::db::DataSource;
use crate::remedy::{Problem, Remedy};
use chrono::{DateTime, Utc};
use std::path::Path;

/// A snapshot of one service that matches the configured seed.
pub(super) struct Candidate {
    pub version: u32,
    /// When its data was taken.
    pub taken_at: DateTime<Utc>,
}

/// The snapshot a run seeds from: the configured name and, when given, the
/// version. `None` when the run is interactive, was told its data source,
/// is for the default workspace, or the project configures no seed. A version
/// that is not a positive number is a [`Problem`] (configuration refuses it,
/// but a `Config` can be built in code): it never reads as "no version", which
/// would select the latest snapshot.
pub(super) fn requested(
    config: &Config,
    options: &SetupOptions,
    is_default: bool,
) -> Result<Option<(String, Option<u32>)>, Problem> {
    if !options.non_interactive || options.data_from.is_some() || is_default {
        return Ok(None);
    }
    let Some(DataSource::Snapshot { name, version }) = config.data.noninteractive_source() else {
        return Ok(None);
    };
    let version = version
        .map(|v| u32::try_from(v).ok().filter(|v| *v > 0))
        .map(|v| {
            v.ok_or_else(|| {
                Problem::new(
                    "data.noninteractive_from: the snapshot version must be a positive number",
                )
            })
        })
        .transpose()?;
    Ok(Some((name, version)))
}

/// The source chosen for a service, and a warning about the choice.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Choice {
    pub source: DataSource,
    pub warning: Option<String>,
}

/// What to do with the snapshot `name` the lookup for service `key` found
/// (`found`): use it (reported when older than `noninteractive_max_age`), or,
/// without it, start empty when `noninteractive_fallback` says so and fail with
/// a [`Problem`] otherwise.
pub(super) fn judge(
    config: &Config,
    root: &Path,
    (key, engine): (&str, Engine),
    name: &str,
    found: Option<Candidate>,
    now: DateTime<Utc>,
) -> Result<Choice, Problem> {
    let Some(found) = found else {
        return match config.data.noninteractive_fallback {
            Some(SeedFallback::Empty) => Ok(Choice {
                source: DataSource::Empty,
                warning: Some(format!(
                    "service '{key}' has no snapshot named '{name}' (data.noninteractive_from); \
                     starting it empty, as data.noninteractive_fallback says"
                )),
            }),
            None => Err(Problem::new(format!(
                "service '{key}' has no snapshot named '{name}', the seed that \
                 data.noninteractive_from names for runs that cannot prompt. Its usual data \
                 source is a live workspace, which such a run does not clone in its place \
                 (set data.noninteractive_fallback = \"empty\" to start the service empty \
                 instead)"
            ))
            .with(Remedy::CreateSeedSnapshot {
                root: root.to_path_buf(),
                key: key.to_string(),
                snapshot: name.to_string(),
                engine,
            })
            .with(Remedy::ChooseDataSource {
                root: root.to_path_buf(),
            })),
        };
    };
    let version = i32::try_from(found.version).map_err(|_| {
        Problem::new(format!(
            "service '{key}' has a snapshot named '{name}' with a version devflow cannot use"
        ))
    })?;
    let source = DataSource::Snapshot {
        name: name.to_string(),
        version: Some(version),
    };
    let stale = config.data.noninteractive_max_age().and_then(|max| {
        let age = now.signed_duration_since(found.taken_at).to_std().ok()?;
        (age > max).then(|| {
            // Whole minutes are plenty for an age that is measured in days.
            let age = std::time::Duration::from_secs(age.as_secs() / 60 * 60);
            format!(
                "the seed snapshot {name}@v{} of service '{key}' is {} old, older than \
                 data.noninteractive_max_age ({}); it is used anyway",
                found.version,
                humantime::format_duration(age),
                humantime::format_duration(max)
            )
        })
    });
    Ok(Choice {
        source,
        warning: stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> Config {
        Config::parse(&format!(
            "[project]\nname = \"p\"\nid = \"6f2a8a3e-7f3c-4a4e-9a52-6a3a0d5f1b11\"\n{extra}"
        ))
        .unwrap()
    }

    fn options(non_interactive: bool, data_from: Option<DataSource>) -> SetupOptions {
        SetupOptions {
            non_interactive,
            data_from,
            ..SetupOptions::default()
        }
    }

    #[test]
    fn only_a_non_interactive_run_without_data_from_is_seeded() {
        let seeded = config("[data]\nnoninteractive_from = \"snapshot:seed\"\n");
        assert_eq!(
            requested(&seeded, &options(true, None), false).unwrap(),
            Some(("seed".to_string(), None))
        );
        assert_eq!(
            requested(&seeded, &options(false, None), false).unwrap(),
            None
        );
        assert_eq!(
            requested(&seeded, &options(true, Some(DataSource::Empty)), false).unwrap(),
            None,
            "--data-from wins"
        );
        assert_eq!(
            requested(&seeded, &options(true, None), true).unwrap(),
            None,
            "the default workspace's root branch is never seeded"
        );
        assert_eq!(
            requested(&config(""), &options(true, None), false).unwrap(),
            None
        );
        let pinned = config("[data]\nnoninteractive_from = \"snapshot:seed@v3\"\n");
        assert_eq!(
            requested(&pinned, &options(true, None), false).unwrap(),
            Some(("seed".to_string(), Some(3)))
        );
    }

    #[test]
    fn a_version_that_is_not_positive_is_an_error_and_never_no_version() {
        // Configuration refuses it; a `Config` built in code must not turn it
        // into "the latest snapshot" either.
        for version in [-1, 0] {
            let mut config = config("[data]\nnoninteractive_from = \"snapshot:seed\"\n");
            config.data.noninteractive_from = Some(format!("snapshot:seed@v{version}"));
            let problem = requested(&config, &options(true, None), false).unwrap_err();
            assert!(
                problem.message.contains("positive number"),
                "{}",
                problem.message
            );
        }
    }

    #[test]
    fn a_missing_snapshot_fails_with_remedies_and_never_picks_a_live_source() {
        let config = config("[data]\nnoninteractive_from = \"snapshot:seed\"\n");
        let root = Path::new("/repo");
        let problem = judge(
            &config,
            root,
            ("sql", Engine::Mysql),
            "seed",
            None,
            Utc::now(),
        )
        .unwrap_err();
        assert!(
            problem.message.contains("'sql'") && problem.message.contains("'seed'"),
            "{}",
            problem.message
        );
        assert!(!problem.message.contains("devflow "), "no command line");
        assert_eq!(
            problem.remedies,
            vec![
                Remedy::CreateSeedSnapshot {
                    root: root.to_path_buf(),
                    key: "sql".into(),
                    snapshot: "seed".into(),
                    engine: Engine::Mysql,
                },
                Remedy::ChooseDataSource {
                    root: root.to_path_buf()
                },
            ]
        );
    }

    #[test]
    fn the_empty_fallback_is_the_only_way_to_start_without_the_snapshot() {
        let config = config(
            "[data]\nnoninteractive_from = \"snapshot:seed\"\n\
             noninteractive_fallback = \"empty\"\n",
        );
        let choice = judge(
            &config,
            Path::new("/repo"),
            ("cache", Engine::Valkey),
            "seed",
            None,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(choice.source, DataSource::Empty);
        let warning = choice.warning.unwrap();
        assert!(
            warning.contains("'cache'") && warning.contains("noninteractive_fallback"),
            "{warning}"
        );
    }

    #[test]
    fn an_old_snapshot_is_used_with_a_warning() {
        let config = config(
            "[data]\nnoninteractive_from = \"snapshot:seed\"\nnoninteractive_max_age = \"7d\"\n",
        );
        let now = Utc::now();
        let found = |days: i64| Candidate {
            version: 2,
            taken_at: now - chrono::Duration::days(days),
        };
        let root = Path::new("/repo");
        let judged = |days| {
            judge(
                &config,
                root,
                ("db", Engine::Postgres),
                "seed",
                Some(found(days)),
                now,
            )
            .unwrap()
        };
        let fresh = judged(1);
        assert_eq!(
            fresh.source,
            DataSource::Snapshot {
                name: "seed".into(),
                version: Some(2)
            }
        );
        assert_eq!(fresh.warning, None);
        let old = judged(9);
        let warning = old.warning.unwrap();
        assert!(
            warning.contains("seed@v2") && warning.contains("9days"),
            "{warning}"
        );
    }
}
