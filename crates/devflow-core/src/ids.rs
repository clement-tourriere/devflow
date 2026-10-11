//! Branch and snapshot ids as people type them.
//!
//! Listings show an id as its first [`SHORT_LEN`] hex characters
//! (`105a6201`), so every command that takes a branch id accepts what the
//! listing shows: a full UUID, or a unique prefix of at least [`SHORT_LEN`]
//! hex characters, with or without dashes. Parsing is syntax only
//! ([`IdRef`]); a prefix is resolved against the ids that exist (the
//! catalog of a container service, the metadata database of PostgreSQL) by
//! [`IdRef::resolve`], so an ambiguous or unknown prefix is an error that
//! names the candidates, and an id is never guessed.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// The length of the id listings show, and the shortest prefix accepted.
pub const SHORT_LEN: usize = 8;

/// The id listings show: the first [`SHORT_LEN`] hex characters.
pub fn short(id: Uuid) -> String {
    id.simple().to_string()[..SHORT_LEN].to_string()
}

/// An id as typed: a whole UUID or a prefix of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdRef {
    /// A whole UUID: nothing to resolve.
    Full(Uuid),
    /// Lowercase hex without dashes, [`SHORT_LEN`] to 31 characters.
    Prefix(String),
}

impl FromStr for IdRef {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let hex: String = text
            .trim()
            .chars()
            .filter(|c| *c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!(
                "'{text}' is not an id: give a UUID or at least {SHORT_LEN} hex characters of one \
                 (the short id the listings show)"
            );
        }
        if hex.len() == 32 {
            return Ok(IdRef::Full(Uuid::parse_str(&hex)?));
        }
        if hex.len() < SHORT_LEN || hex.len() > 32 {
            bail!(
                "'{text}' is not an id: a prefix needs at least {SHORT_LEN} hex characters (and a \
                 UUID has 32)"
            );
        }
        Ok(IdRef::Prefix(hex))
    }
}

impl fmt::Display for IdRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdRef::Full(id) => write!(f, "{id}"),
            IdRef::Prefix(prefix) => f.write_str(prefix),
        }
    }
}

impl From<Uuid> for IdRef {
    fn from(id: Uuid) -> Self {
        IdRef::Full(id)
    }
}

impl IdRef {
    /// The id this names among `ids` (the branches, or snapshots, that
    /// exist). A whole UUID is returned as it is: the caller reports an id
    /// that does not exist in its own words. A prefix must match exactly one
    /// id; `what` ("branch") is for the error.
    pub fn resolve(&self, what: &str, ids: impl IntoIterator<Item = Uuid>) -> Result<Uuid> {
        let prefix = match self {
            IdRef::Full(id) => return Ok(*id),
            IdRef::Prefix(prefix) => prefix,
        };
        let mut found: Vec<Uuid> = ids
            .into_iter()
            .filter(|id| id.simple().to_string().starts_with(prefix.as_str()))
            .collect();
        found.sort();
        found.dedup();
        match found.as_slice() {
            [] => bail!("no {what} has an id starting with '{prefix}'"),
            [only] => Ok(*only),
            many => bail!(
                "'{prefix}' is ambiguous: the {what} ids {} all start with it; give more characters",
                distinct_short_ids(many).join(", ")
            ),
        }
    }
}

/// The ids shortened to the least length (at least [`SHORT_LEN`]) that tells
/// them apart, so an error naming candidates names something one can type.
fn distinct_short_ids(ids: &[Uuid]) -> Vec<String> {
    let full: Vec<String> = ids.iter().map(|id| id.simple().to_string()).collect();
    let len = (SHORT_LEN..=32)
        .find(|len| {
            let mut seen: Vec<&str> = full.iter().map(|id| &id[..*len]).collect();
            seen.sort_unstable();
            seen.dedup();
            seen.len() == full.len()
        })
        .unwrap_or(32);
    full.iter().map(|id| id[..len].to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> Uuid {
        text.parse().unwrap()
    }

    #[test]
    fn what_listings_show_parses_with_or_without_dashes() {
        let full = id("105a6201-aaaa-4bbb-8ccc-0123456789ab");
        assert_eq!(short(full), "105a6201");
        assert_eq!(
            "105a6201".parse::<IdRef>().unwrap(),
            IdRef::Prefix("105a6201".into())
        );
        assert_eq!(
            "105A6201-AAAA".parse::<IdRef>().unwrap(),
            IdRef::Prefix("105a6201aaaa".into()),
            "case and dashes do not matter"
        );
        assert_eq!(
            "105a6201aaaa4bbb8ccc0123456789ab".parse::<IdRef>().unwrap(),
            IdRef::Full(full)
        );
        assert_eq!(
            "105a6201-aaaa-4bbb-8ccc-0123456789ab"
                .parse::<IdRef>()
                .unwrap(),
            IdRef::Full(full)
        );
    }

    #[test]
    fn a_prefix_shorter_than_the_listing_or_not_hex_is_refused() {
        for bad in [
            "",
            "-",
            "105a620",
            "105a-620",
            "not-a-uuid",
            "105a620g",
            "105a6201aaaa4bbb8ccc0123456789abc",
        ] {
            let error = bad.parse::<IdRef>().unwrap_err().to_string();
            assert!(error.contains("not an id"), "{bad:?}: {error}");
        }
    }

    #[test]
    fn a_prefix_resolves_to_the_one_id_it_starts() {
        let a = id("105a6201-aaaa-4bbb-8ccc-0123456789ab");
        let b = id("3f0c9d11-aaaa-4bbb-8ccc-0123456789ab");
        let all = [a, b];
        let by = |text: &str| text.parse::<IdRef>().unwrap().resolve("branch", all);
        assert_eq!(by("105a6201").unwrap(), a);
        assert_eq!(by("105a6201-aaaa-4").unwrap(), a);
        assert_eq!(by("3F0C9D11").unwrap(), b);
        // A whole UUID is not looked up: the caller reports a missing id.
        assert_eq!(
            by("deadbeef-aaaa-4bbb-8ccc-0123456789ab").unwrap(),
            id("deadbeef-aaaa-4bbb-8ccc-0123456789ab")
        );
        let error = by("deadbeef").unwrap_err().to_string();
        assert!(
            error.contains("no branch") && error.contains("deadbeef"),
            "{error}"
        );
    }

    #[test]
    fn an_ambiguous_prefix_names_candidates_that_can_be_told_apart() {
        let a = id("105a6201-aaaa-4bbb-8ccc-0123456789ab");
        let b = id("105a6201-bbbb-4bbb-8ccc-0123456789ab");
        let c = id("3f0c9d11-aaaa-4bbb-8ccc-0123456789ab");
        let error = "105a6201"
            .parse::<IdRef>()
            .unwrap()
            .resolve("branch", [a, b, c, a])
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");
        // Both share the first eight characters, so the error goes on until
        // they differ; the unrelated branch is not named.
        assert!(
            error.contains("105a6201a") && error.contains("105a6201b"),
            "{error}"
        );
        assert!(!error.contains("3f0c9d11"), "{error}");
    }
}
