//! Does the Docker engine's volume storage share blocks between copies?
//!
//! Measured inside the Docker VM, where the volumes live: write 64 MiB into
//! a scratch volume, copy it with `cp --reflink=always`, and compare the
//! guest's free space before and after. The result is `verified` (the clone
//! took a small fraction of the file), `copy` (the filesystem refused to share
//! blocks, or the copy took the space), or `unknown` (the measurement was
//! disturbed or could not be made). It never assumes.
//!
//! One result per Docker daemon (its `docker info` ID), cached in the data dir
//! so `devflow doctor` and `devflow service status` do not rerun it.
use super::docker::{self, HELPER_IMAGE, HelperOutcome, HelperSpec};
use crate::config::CowCapability;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Size of the probe file.
const PROBE_BYTES: i64 = 64 * 1024 * 1024;
/// A cached result older than this is measured again.
const MAX_AGE_DAYS: i64 = 30;

/// One measurement, for one Docker daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CowProbe {
    pub capability: CowCapability,
    pub probe_bytes: i64,
    /// Free space the clone took in the guest, in bytes.
    pub growth_bytes: i64,
    /// Why the result is `copy` or `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub measured_at: DateTime<Utc>,
    /// The Docker context the daemon was reached through when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    /// Results by `docker info` ID.
    #[serde(default)]
    daemons: BTreeMap<String, CowProbe>,
}

fn cache_path() -> Result<std::path::PathBuf> {
    Ok(crate::paths::data_dir()?.join("services").join("cow.json"))
}

fn read_cache() -> Cache {
    cache_path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The cached result for `daemon`, if it is recent.
pub fn cached(daemon: &str) -> Option<CowProbe> {
    let found = read_cache().daemons.get(daemon).cloned()?;
    (Utc::now() - found.measured_at < ChronoDuration::days(MAX_AGE_DAYS)).then_some(found)
}

/// The cached result, or a fresh measurement (stored). With `refresh` the
/// measurement is always made again. Never fails: a measurement that could
/// not be made is `unknown` with the reason.
pub fn capability(daemon: &str, refresh: bool) -> CowProbe {
    if !refresh && let Some(found) = cached(daemon) {
        return found;
    }
    let probe = measure();
    let _ = store(daemon, &probe);
    probe
}

fn store(daemon: &str, probe: &CowProbe) -> Result<()> {
    let path = cache_path()?;
    crate::fsutil::with_lock(&path.with_extension("lock"), || {
        let mut cache = read_cache();
        cache.daemons.insert(daemon.to_string(), probe.clone());
        crate::fsutil::write_atomic(&path, &serde_json::to_vec_pretty(&cache)?, false)
    })
}

/// What the helper script printed.
#[derive(Debug, PartialEq, Eq)]
struct Readings {
    first_free: i64,
    before_free: i64,
    after_free: i64,
    /// `cp --reflink=always` succeeded.
    cloned: bool,
    /// Its error text when it did not.
    error: String,
}

fn parse_readings(out: &str) -> Option<Readings> {
    let line = out.lines().find(|l| l.starts_with("probe "))?;
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    let rest = line.strip_prefix("probe ")?;
    let (numbers, error) = rest.split_once(" error=").unwrap_or((rest, ""));
    for part in numbers.split_whitespace() {
        let (k, v) = part.split_once('=')?;
        fields.insert(k, v);
    }
    let kib = |key: &str| -> Option<i64> { Some(fields.get(key)?.parse::<i64>().ok()? * 1024) };
    Some(Readings {
        first_free: kib("first")?,
        before_free: kib("before")?,
        after_free: kib("after")?,
        cloned: *fields.get("cloned")? == "yes",
        error: error.trim().to_string(),
    })
}

/// Classify the readings. `Copy` means reflink is not available (refused, or
/// the copy took the space); `Unknown` is for a disturbed measurement.
fn classify(readings: &Readings) -> (CowCapability, i64, Option<String>) {
    let drift = (readings.first_free - readings.before_free).abs();
    if !readings.cloned {
        let text = readings.error.to_lowercase();
        let refused = [
            "not supported",
            "invalid argument",
            "cross-device",
            "not permitted",
        ]
        .iter()
        .any(|marker| text.contains(marker));
        return if refused {
            (
                CowCapability::Copy,
                0,
                Some(format!(
                    "the volume filesystem refused a reflink: {}",
                    readings.error
                )),
            )
        } else {
            (
                CowCapability::Unknown,
                0,
                Some(format!("the reflink copy failed: {}", readings.error)),
            )
        };
    }
    if drift * 20 > PROBE_BYTES {
        return (
            CowCapability::Unknown,
            0,
            Some(format!(
                "free space moved by {drift} bytes during the baseline (other activity on the \
                 filesystem)"
            )),
        );
    }
    let growth = (readings.before_free - readings.after_free).max(0);
    if growth * 10 < PROBE_BYTES {
        (CowCapability::Verified, growth, None)
    } else {
        (
            CowCapability::Copy,
            growth,
            Some(format!("the reflink copy took {growth} bytes")),
        )
    }
}

/// Measure now. The scratch volume and the helper are removed afterwards.
fn measure() -> CowProbe {
    let context = docker::context_name();
    let mut probe = CowProbe {
        capability: CowCapability::Unknown,
        probe_bytes: PROBE_BYTES,
        growth_bytes: 0,
        note: None,
        measured_at: Utc::now(),
        context,
    };
    match run_probe() {
        Ok(readings) => {
            let (capability, growth, note) = classify(&readings);
            probe.capability = capability;
            probe.growth_bytes = growth;
            probe.note = note;
        }
        Err(error) => probe.note = Some(format!("{error:#}")),
    }
    probe
}

const SCRIPT: &str = r#"set -eu
cd /v
free() { sync; df -Pk /v | awk 'NR==2 {print $4}'; }
head -c 67108864 /dev/urandom > probe.a
first=$(free)
sleep 1
before=$(free)
if cp --reflink=always probe.a probe.b 2>cp.err; then cloned=yes; else cloned=no; fi
after=$(free)
echo "probe first=$first before=$before after=$after cloned=$cloned error=$(tr '\n' ' ' < cp.err)"
"#;

fn run_probe() -> Result<Readings> {
    let tag = Uuid::new_v4().simple().to_string();
    let volume = format!("devflow-probe-{}", &tag[..12]);
    let labels = vec![("dev.devflow.probe".to_string(), tag.clone())];
    docker::create_volume(&volume, &labels)?;
    let result = (|| -> Result<Readings> {
        let helper = docker::create_helper(&HelperSpec {
            name: format!("devflow-probe-{}", &tag[..12]),
            image: HELPER_IMAGE.into(),
            mounts: vec![(volume.clone(), "/v".into(), false)],
            env: Vec::new(),
            labels,
            script: SCRIPT.into(),
        })?;
        let outcome = docker::run_helper(&helper, Instant::now() + Duration::from_secs(120));
        let _ = docker::kill_and_confirm(&helper, Duration::from_secs(10));
        match outcome? {
            HelperOutcome::Exited { code: 0, stdout } => parse_readings(&stdout)
                .with_context(|| format!("unreadable probe output: {}", stdout.trim())),
            HelperOutcome::Exited { code, stdout } => {
                anyhow::bail!("the probe helper exited with {code}: {}", stdout.trim())
            }
            HelperOutcome::TimedOut => anyhow::bail!("the probe did not finish in 120 seconds"),
        }
    })();
    let _ = docker::remove_volume(&volume);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readings(first: i64, before: i64, after: i64, cloned: bool, error: &str) -> Readings {
        Readings {
            first_free: first,
            before_free: before,
            after_free: after,
            cloned,
            error: error.into(),
        }
    }

    #[test]
    fn the_helper_output_is_parsed() {
        let out = "noise\nprobe first=1000 before=1000 after=990 cloned=yes error=\n";
        assert_eq!(
            parse_readings(out).unwrap(),
            readings(1000 * 1024, 1000 * 1024, 990 * 1024, true, "")
        );
        let failed = "probe first=5 before=5 after=5 cloned=no error=cp: failed to clone 'probe.b': Operation not supported \n";
        let parsed = parse_readings(failed).unwrap();
        assert!(!parsed.cloned);
        assert!(parsed.error.contains("Operation not supported"));
        assert!(parse_readings("no probe line").is_none());
        assert!(parse_readings("probe first=x before=1 after=1 cloned=yes error=").is_none());
    }

    #[test]
    fn a_clone_that_took_little_space_is_verified() {
        let tenth = PROBE_BYTES / 10;
        let (capability, growth, note) = classify(&readings(
            10 * PROBE_BYTES,
            10 * PROBE_BYTES,
            10 * PROBE_BYTES - 4096,
            true,
            "",
        ));
        assert_eq!(capability, CowCapability::Verified);
        assert_eq!(growth, 4096);
        assert!(note.is_none());
        // Just under a tenth is still sharing; a tenth or more is a copy.
        let (capability, ..) = classify(&readings(
            10 * PROBE_BYTES,
            10 * PROBE_BYTES,
            10 * PROBE_BYTES - tenth + 1,
            true,
            "",
        ));
        assert_eq!(capability, CowCapability::Verified);
        let (capability, growth, note) = classify(&readings(
            10 * PROBE_BYTES,
            10 * PROBE_BYTES,
            10 * PROBE_BYTES - PROBE_BYTES,
            true,
            "",
        ));
        assert_eq!(capability, CowCapability::Copy);
        assert_eq!(growth, PROBE_BYTES);
        assert!(note.unwrap().contains("took"));
    }

    #[test]
    fn a_refused_reflink_is_copy_and_a_disturbed_measurement_is_unknown() {
        let (capability, _, note) = classify(&readings(
            1,
            1,
            1,
            false,
            "cp: failed to clone 'probe.b' from 'probe.a': Operation not supported",
        ));
        assert_eq!(capability, CowCapability::Copy);
        assert!(note.unwrap().contains("refused a reflink"));
        // A failure that says nothing about reflinks does not decide.
        let (capability, ..) = classify(&readings(1, 1, 1, false, "cp: No space left on device"));
        assert_eq!(capability, CowCapability::Unknown);
        // Free space moved a lot before the copy: nothing can be concluded.
        let (capability, _, note) = classify(&readings(
            10 * PROBE_BYTES,
            10 * PROBE_BYTES - PROBE_BYTES,
            10 * PROBE_BYTES - PROBE_BYTES,
            true,
            "",
        ));
        assert_eq!(capability, CowCapability::Unknown);
        assert!(note.unwrap().contains("other activity"));
    }
}
