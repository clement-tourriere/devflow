//! YAML parsing for untrusted repositories.
//!
//! Compose files and workspace manifests come from whatever repository is
//! being scanned, so parsing is bounded: `serde-saphyr` is panic-free and the
//! limits below stop alias-expansion bombs ("billion laughs") long before
//! they cost real time or memory.
use super::repo::Repo;
use serde_json::Value;

/// Parse the first YAML document of `text` into a JSON value (anchors and
/// merge keys resolved). On failure a warning is recorded against `rel`.
pub(crate) fn parse(repo: &Repo, rel: &str, text: &str) -> Option<Value> {
    let options = serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_events: 200_000,
            max_aliases: 2_000,
            max_anchors: 2_000,
            max_recorded_anchor_events: 200_000,
        },
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 50_000,
            max_replay_stack_depth: 16,
        },
    };
    match serde_saphyr::from_str_with_options::<Value>(text, options) {
        Ok(value) => Some(value),
        Err(e) => {
            let message = e.to_string();
            let first = message.lines().next().unwrap_or("parse error");
            repo.warn(rel, format!("invalid YAML: {first}"));
            None
        }
    }
}
