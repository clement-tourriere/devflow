//! The tracked-path set of a Git index file.
//!
//! The scan needs one fact from the index: whether a path is tracked (a
//! tracked file is never "ignored", whatever the rules say). The bytes come
//! from the safe reader, so this is a small bounded parser instead of a
//! libgit2 call that would open `.git/index` by itself. Versions 2, 3 and 4,
//! SHA-1 and SHA-256 object names are read.
//!
//! # The checksum decides the hash size
//!
//! An index does not say which hash it uses (the config that does is not
//! read), but its last 20 or 32 bytes are the checksum of everything before
//! them. The trailer is verified for each size and only a size that verifies
//! is parsed. A file that parses under one size only by taking bytes of the
//! other layout for a trailer (a required extension swallowed whole, say) has
//! no matching checksum under that size: it is damaged, as Git finds.
//!
//! A trailer of all zeros means the checksum was skipped (`index.skipHash`,
//! which `feature.manyFiles` turns on), and Git accepts that unverified. The
//! layout is then read for each size the zeros allow, and the index is used
//! only if the readings agree: when one reads and the other does not, the
//! answer is unknown, never whichever happened to succeed.
//!
//! # Not understood, so unknown
//!
//! Git's index format marks an extension as optional when its four-byte
//! signature starts with `A`-`Z` (`TREE`, `REUC`, `UNTR`, `FSMN`, ...) and as
//! required otherwise. The required ones change what the entries mean: `link`
//! (a split index keeps most of its entries in a separate shared file) and
//! `sdir` (a sparse index stores directories in place of their files). The
//! parser cannot honor them, so any required extension makes the whole index
//! unreadable instead of yielding a partial set of tracked paths.
//!
//! # Bounded work
//!
//! Version 4 stores each path as "drop N bytes of the previous path, append
//! this", so a few kilobytes of index can describe megabytes of paths. The
//! expansion is therefore checked before anything is allocated: a path is at
//! most [`MAX_PATH_BYTES`] long, and all paths together stay within a byte
//! budget the caller supplies (the scan's remaining read budget).
use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// Fixed part of an entry before the object name: ten 32-bit fields.
const STAT_BYTES: usize = 40;
/// File mode bits of a directory entry, which only a sparse index contains.
const MODE_DIR: u32 = 0o040_000;
const MODE_TYPE_MASK: u32 = 0o170_000;
/// Longest path accepted, expanded; a longer one makes the index unreadable.
const MAX_PATH_BYTES: usize = 4096;

/// Sizes of an object name, and so of the checksum trailer: SHA-1, SHA-256.
const HASH_SIZES: [usize; 2] = [20, 32];

const DAMAGED: &str = "unsupported or damaged index";
const AMBIGUOUS: &str = "index with no checksum that reads differently for each hash size";
const SPLIT_OR_SPARSE: &str = "split or sparse index";
const TOO_BIG: &str = "index paths exceed the limits of the scan";

/// Why an attempt to read the index stopped, ordered by how informative it is
/// for the message (a wrong guess of the hash size looks like `Damaged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stop {
    /// Malformed.
    Damaged,
    /// A required extension (`link`, `sdir`) or a directory entry.
    Unsupported,
    /// A path, or the paths together, exceeded the bounds.
    Limit,
}

impl Stop {
    fn reason(self) -> &'static str {
        match self {
            Stop::Damaged => DAMAGED,
            Stop::Unsupported => SPLIT_OR_SPARSE,
            Stop::Limit => TOO_BIG,
        }
    }
}

/// Paths with an index entry, or the reason the index cannot be read
/// completely (damaged, unknown version, split, sparse, paths beyond the
/// bounds).
///
/// `budget` is the number of expanded path bytes the caller allows; it is
/// reduced by what the parse used.
pub(super) fn tracked_paths(
    bytes: &[u8],
    budget: &mut u64,
) -> Result<HashSet<Vec<u8>>, &'static str> {
    if bytes.len() < 12 || &bytes[..4] != b"DIRC" {
        return Err(DAMAGED);
    }
    let version = be32(bytes, 4).ok_or(DAMAGED)?;
    if !(2..=4).contains(&version) {
        return Err(DAMAGED);
    }
    let count = be32(bytes, 8).ok_or(DAMAGED)? as usize;
    // An entry is at least a stat block, an object name and a flags field.
    if count > bytes.len() / (STAT_BYTES + 20 + 2) {
        return Err(DAMAGED);
    }
    // A verified checksum fixes the hash size: a second size cannot also
    // verify without the file being a fixed point of a hash. Without one, all
    // that is left is a skipped checksum, which fixes nothing.
    let mut skipped = Vec::new();
    for hash in HASH_SIZES {
        match trailer(bytes, hash) {
            Trailer::Verified => {
                return parse(bytes, version, count, hash, budget).map_err(Stop::reason);
            }
            Trailer::Skipped => skipped.push(hash),
            Trailer::Mismatch => {}
        }
    }
    match skipped.as_slice() {
        [] => Err(DAMAGED),
        [hash] => parse(bytes, version, count, *hash, budget).map_err(Stop::reason),
        _ => {
            // Both sizes are possible. Each reading gets the whole budget and
            // the scan is charged for the one that used more.
            let (mut sha1_left, mut sha256_left) = (*budget, *budget);
            let sha1 = parse(bytes, version, count, 20, &mut sha1_left);
            let sha256 = parse(bytes, version, count, 32, &mut sha256_left);
            *budget -= (*budget - sha1_left).max(*budget - sha256_left);
            match (sha1, sha256) {
                (Ok(a), Ok(b)) if a == b => Ok(a),
                // One reading works where the other does not: which one Git
                // would use depends on a config the scan does not read.
                (Ok(_), _) | (_, Ok(_)) => Err(AMBIGUOUS),
                (Err(a), Err(b)) => Err(a.max(b).reason()),
            }
        }
    }
}

/// How the trailer of an index relates to the bytes before it, for one hash size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trailer {
    /// The checksum of everything before the trailer.
    Verified,
    /// All zeros: the checksum was not computed (`index.skipHash`).
    Skipped,
    /// Anything else, or no room for a header and a trailer of this size.
    Mismatch,
}

fn trailer(bytes: &[u8], hash: usize) -> Trailer {
    let Some(split) = bytes.len().checked_sub(hash).filter(|split| *split >= 12) else {
        return Trailer::Mismatch;
    };
    let (body, tail) = bytes.split_at(split);
    if tail.iter().all(|b| *b == 0) {
        return Trailer::Skipped;
    }
    let matches = if hash == 20 {
        sha1_smol::Sha1::from(body).digest().bytes().as_slice() == tail
    } else {
        Sha256::digest(body).as_slice() == tail
    };
    if matches {
        Trailer::Verified
    } else {
        Trailer::Mismatch
    }
}

fn parse(
    bytes: &[u8],
    version: u32,
    count: usize,
    hash: usize,
    budget: &mut u64,
) -> Result<HashSet<Vec<u8>>, Stop> {
    let end = bytes.len().checked_sub(hash).ok_or(Stop::Damaged)?;
    let mut pos = 12;
    let mut previous: Vec<u8> = Vec::new();
    let mut paths = HashSet::with_capacity(count);
    for _ in 0..count {
        let start = pos;
        let mode = be32(bytes, pos + 24).ok_or(Stop::Damaged)?;
        if mode & MODE_TYPE_MASK == MODE_DIR {
            return Err(Stop::Unsupported);
        }
        pos += STAT_BYTES + hash;
        let flags = u16::from_be_bytes([
            *bytes.get(pos).ok_or(Stop::Damaged)?,
            *bytes.get(pos + 1).ok_or(Stop::Damaged)?,
        ]);
        pos += 2;
        if version >= 3 && flags & 0x4000 != 0 {
            pos += 2;
        }
        if pos > end {
            return Err(Stop::Damaged);
        }
        if version == 4 {
            let tail = bytes.get(pos..end).ok_or(Stop::Damaged)?;
            let (strip, used) = varint(tail).ok_or(Stop::Damaged)?;
            pos += used;
            let suffix = nul_terminated(bytes, pos, end)?;
            let keep = previous.len().checked_sub(strip).ok_or(Stop::Damaged)?;
            // Decide before building the path: this is where a few bytes of
            // index turn into a long one.
            spend(budget, keep + suffix)?;
            previous.truncate(keep);
            previous.extend_from_slice(&bytes[pos..pos + suffix]);
            pos += suffix + 1;
            paths.insert(previous.clone());
        } else {
            let length = nul_terminated(bytes, pos, end)?;
            spend(budget, length)?;
            paths.insert(bytes[pos..pos + length].to_vec());
            // NUL padding up to a multiple of eight bytes, at least one NUL.
            pos += length + 1;
            pos += (8 - (pos - start) % 8) % 8;
        }
        if pos > end {
            return Err(Stop::Damaged);
        }
    }
    // Extensions: `signature(4) size(4) data`.
    while pos < end {
        let header = bytes.get(pos..pos + 8).ok_or(Stop::Damaged)?;
        // Only an optional extension (uppercase signature) may be skipped.
        if !header[0].is_ascii_uppercase() {
            return Err(Stop::Unsupported);
        }
        let size = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
        pos = pos
            .checked_add(8)
            .and_then(|p| p.checked_add(size))
            .ok_or(Stop::Damaged)?;
    }
    if pos == end {
        Ok(paths)
    } else {
        Err(Stop::Damaged)
    }
}

/// Length of the NUL-terminated string at `pos`, looking no further than the
/// longest path allowed so that a missing terminator is found cheaply.
fn nul_terminated(bytes: &[u8], pos: usize, end: usize) -> Result<usize, Stop> {
    let window = bytes.get(pos..end).ok_or(Stop::Damaged)?;
    let window = &window[..window.len().min(MAX_PATH_BYTES + 1)];
    match window.iter().position(|b| *b == 0) {
        Some(length) => Ok(length),
        None if window.len() > MAX_PATH_BYTES => Err(Stop::Limit),
        None => Err(Stop::Damaged),
    }
}

/// Charge a path of `length` expanded bytes against the budget.
fn spend(budget: &mut u64, length: usize) -> Result<(), Stop> {
    if length > MAX_PATH_BYTES {
        return Err(Stop::Limit);
    }
    *budget = budget.checked_sub(length as u64).ok_or(Stop::Limit)?;
    Ok(())
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Git's offset-encoded varint (version 4 path prefix compression): the value
/// and the bytes used.
fn varint(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut used = 0;
    let mut byte = *bytes.get(used)?;
    used += 1;
    let mut value = (byte & 0x7f) as usize;
    while byte & 0x80 != 0 {
        byte = *bytes.get(used)?;
        used += 1;
        value = value.checked_add(1)?.checked_mul(128)? | (byte & 0x7f) as usize;
    }
    Some((value, used))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A byte budget that no ordinary test index comes near.
    fn ample() -> u64 {
        32 * 1024 * 1024
    }

    fn tracked(bytes: &[u8]) -> Option<HashSet<Vec<u8>>> {
        tracked_paths(bytes, &mut ample()).ok()
    }

    /// One version 4 entry that drops `strip` (< 128) bytes of the previous
    /// path and appends `suffix`.
    fn v4_entry(hash: usize, strip: u8, suffix: &[u8]) -> Vec<u8> {
        let mut e = vec![0u8; STAT_BYTES + hash + 2];
        e[24..28].copy_from_slice(&0o100_644u32.to_be_bytes());
        e.push(strip);
        e.extend_from_slice(suffix);
        e.push(0);
        e
    }

    /// The checksum Git ends an index with: SHA-1 or SHA-256 of the contents.
    fn checksum(body: &[u8], hash: usize) -> Vec<u8> {
        if hash == 20 {
            sha1_smol::Sha1::from(body).digest().bytes().to_vec()
        } else {
            Sha256::digest(body).to_vec()
        }
    }

    /// `body` followed by its checksum, as a real index ends.
    fn seal(mut body: Vec<u8>, hash: usize) -> Vec<u8> {
        let sum = checksum(&body, hash);
        body.extend_from_slice(&sum);
        body
    }

    /// `body` followed by the zeros of an index written with `index.skipHash`.
    fn skip_hash(mut body: Vec<u8>, hash: usize) -> Vec<u8> {
        body.extend(std::iter::repeat_n(0, hash));
        body
    }

    /// An index after an edit: the same bytes with a correct trailer.
    fn reseal(mut index: Vec<u8>, hash: usize) -> Vec<u8> {
        index.truncate(index.len() - hash);
        seal(index, hash)
    }

    fn v4_index(hash: usize, entries: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"DIRC".to_vec();
        out.extend_from_slice(&4u32.to_be_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        entries.iter().for_each(|e| out.extend_from_slice(e));
        seal(out, hash)
    }

    /// One 4000-byte path and then `more` paths that each rewrite its last two
    /// bytes: a small index that expands to `more * 4000` bytes of paths.
    fn expanding_index(hash: usize, more: usize) -> Vec<u8> {
        let mut entries = vec![v4_entry(hash, 0, &[b'a'; 4000])];
        for i in 0..more {
            entries.push(v4_entry(
                hash,
                2,
                &[1 + (i % 250) as u8, 1 + (i / 250) as u8],
            ));
        }
        v4_index(hash, &entries)
    }

    /// One index entry with a recognizable mode and no stat data.
    fn entry(version: u32, hash: usize, path: &[u8], previous: &[u8], mode: u32) -> Vec<u8> {
        let mut e = vec![0u8; STAT_BYTES + hash + 2];
        e[24..28].copy_from_slice(&mode.to_be_bytes());
        let name_len = path.len().min(0xfff) as u16;
        e[STAT_BYTES + hash..].copy_from_slice(&name_len.to_be_bytes());
        if version == 4 {
            let common = previous
                .iter()
                .zip(path)
                .take_while(|(a, b)| a == b)
                .count();
            let strip = previous.len() - common;
            // Offset-encoded varint of `strip` (small values only).
            assert!(strip < 128);
            e.push(strip as u8);
            e.extend_from_slice(&path[common..]);
            e.push(0);
        } else {
            e.extend_from_slice(path);
            e.push(0);
            while !e.len().is_multiple_of(8) {
                e.push(0);
            }
        }
        e
    }

    /// An index with its checksum.
    fn index(version: u32, hash: usize, paths: &[&[u8]], extension: Option<&[u8]>) -> Vec<u8> {
        seal(unsealed(version, hash, paths, extension), hash)
    }

    /// An index without the trailer.
    fn unsealed(version: u32, hash: usize, paths: &[&[u8]], extension: Option<&[u8]>) -> Vec<u8> {
        let mut out = b"DIRC".to_vec();
        out.extend_from_slice(&version.to_be_bytes());
        out.extend_from_slice(&(paths.len() as u32).to_be_bytes());
        let mut previous: &[u8] = b"";
        for path in paths {
            out.extend(entry(version, hash, path, previous, 0o100_644));
            previous = path;
        }
        if let Some(extension) = extension {
            out.extend_from_slice(extension);
        }
        out
    }

    fn set(paths: &[&[u8]]) -> HashSet<Vec<u8>> {
        paths.iter().map(|p| p.to_vec()).collect()
    }

    #[test]
    fn versions_and_hash_sizes() {
        let paths: [&[u8]; 4] = [b".env", b"src/main.rs", b"src/lib.rs", b"zzz"];
        // Version 4 compresses against the previous path, so keep them sorted.
        let sorted: [&[u8]; 4] = [b".env", b"src/lib.rs", b"src/main.rs", b"zzz"];
        for (version, hash) in [(2, 20), (3, 20), (2, 32), (4, 20), (4, 32)] {
            let list = if version == 4 { &sorted } else { &paths };
            assert_eq!(
                tracked(&index(version, hash, list, None)),
                Some(set(list)),
                "version {version}, {hash}-byte hashes"
            );
        }
    }

    #[test]
    fn extensions_are_skipped_but_a_sparse_index_is_not_understood() {
        let mut tree = b"TREE".to_vec();
        tree.extend_from_slice(&3u32.to_be_bytes());
        tree.extend_from_slice(b"abc");
        assert_eq!(
            tracked(&index(2, 20, &[b"a"], Some(&tree))),
            Some(set(&[b"a"]))
        );

        let mut sparse = b"sdir".to_vec();
        sparse.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(tracked(&index(2, 20, &[b"a"], Some(&sparse))), None);
    }

    #[test]
    fn damaged_input_is_unknown_not_empty() {
        assert_eq!(tracked(b""), None);
        assert_eq!(tracked(b"DIRC"), None);
        assert_eq!(tracked(b"NOPE\0\0\0\x02\0\0\0\0"), None);
        let mut good = index(2, 20, &[b"a", b"b"], None);
        good.truncate(good.len() - 30);
        assert_eq!(tracked(&good), None);
        let mut lying = index(2, 20, &[b"a"], None);
        lying[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(tracked(&lying), None);
        let mut unsupported = index(2, 20, &[b"a"], None);
        unsupported[4..8].copy_from_slice(&9u32.to_be_bytes());
        assert_eq!(tracked(&unsupported), None);
    }

    /// The index libgit2 writes, in every version it can write, with paths that
    /// share prefixes (version 4 compresses them) and non-ASCII names.
    #[test]
    fn reads_what_libgit2_writes() {
        let names = [
            ".env",
            "Cargo.toml",
            "src/lib.rs",
            "src/main.rs",
            "src/deep/nested/mod.rs",
            "src/deep/nested/more.rs",
            "src/deeper.rs",
            "with space/a b.txt",
            "\u{fc}n\u{ef}/x",
        ];
        for version in [2, 3, 4] {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = git2::Repository::init(dir.path()).expect("init");
            let mut index = repo.index().expect("index");
            index.set_version(version).expect("version");
            for name in names {
                let path = dir.path().join(name);
                std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                std::fs::write(&path, name).expect("write");
                index.add_path(std::path::Path::new(name)).expect("add");
            }
            index.write().expect("write index");
            let bytes = std::fs::read(dir.path().join(".git/index")).expect("read index");

            let expected: HashSet<Vec<u8>> = names.iter().map(|n| n.as_bytes().to_vec()).collect();
            assert_eq!(tracked(&bytes), Some(expected), "version {version}");
        }
    }

    #[test]
    fn an_index_without_entries_is_empty() {
        assert_eq!(tracked(&index(2, 20, &[], None)), Some(set(&[])));
    }

    // -- review round 2, finding 3: bounded expansion ---------------------------------

    #[test]
    fn r2_finding_3_a_path_beyond_the_length_cap_is_refused_in_every_version() {
        let long = vec![b'p'; MAX_PATH_BYTES + 1];
        for version in [2, 3, 4] {
            let bytes = index(version, 20, &[&long], None);
            assert_eq!(
                tracked_paths(&bytes, &mut ample()),
                Err(TOO_BIG),
                "version {version}"
            );
        }
        // The cap itself is allowed.
        let longest = vec![b'p'; MAX_PATH_BYTES];
        for version in [2, 4] {
            let bytes = index(version, 20, &[&longest], None);
            assert_eq!(tracked(&bytes), Some(set(&[&longest])), "version {version}");
        }
        // A terminator that never comes is found without scanning the file.
        let mut no_nul = index(2, 20, &[b"a"], None);
        no_nul.splice(12 + 62..12 + 64, std::iter::repeat_n(b'x', 20_000));
        assert!(tracked_paths(&reseal(no_nul, 20), &mut ample()).is_err());
    }

    #[test]
    fn r2_finding_3_prefix_compressed_paths_are_charged_before_they_are_built() {
        let bytes = expanding_index(20, 300);
        assert!(bytes.len() < 30_000, "{} bytes of index", bytes.len());

        // 301 paths of about 4000 bytes: 1.2 MB of expanded paths.
        let mut budget = 2 * 1024 * 1024;
        let paths = tracked_paths(&bytes, &mut budget).expect("within budget");
        assert_eq!(paths.len(), 301);
        let expanded: usize = paths.iter().map(Vec::len).sum();
        assert_eq!(budget, 2 * 1024 * 1024 - expanded as u64);

        // The same index with a smaller budget stops, and never spends more
        // than it was given.
        let mut small = 1024 * 1024;
        assert_eq!(tracked_paths(&bytes, &mut small), Err(TOO_BIG));
        assert!(small <= 1024 * 1024);

        // A pure strip beyond the previous path is damage, not expansion.
        let backwards = v4_index(20, &[v4_entry(20, 5, b"x")]);
        assert_eq!(tracked_paths(&backwards, &mut ample()), Err(DAMAGED));
    }

    #[test]
    fn r2_finding_3_the_budget_applies_to_both_hash_sizes() {
        for hash in [20, 32] {
            let bytes = expanding_index(hash, 300);
            assert_eq!(
                tracked_paths(&bytes, &mut (1024 * 1024)),
                Err(TOO_BIG),
                "{hash}-byte hashes"
            );
            assert_eq!(
                tracked_paths(&bytes, &mut (2 * 1024 * 1024))
                    .map(|paths| paths.len())
                    .map_err(|_| ()),
                Ok(301),
                "{hash}-byte hashes"
            );
        }
    }

    // -- review round 2, finding 6: split and other required extensions ---------------

    #[test]
    fn r2_finding_6_a_split_index_is_unknown_not_partial() {
        // `git update-index --split-index` leaves a `link` extension and keeps
        // most entries in a shared file the parser does not read.
        let mut link = b"link".to_vec();
        link.extend_from_slice(&40u32.to_be_bytes());
        link.extend_from_slice(&[0x11; 20]);
        link.extend_from_slice(&[0; 20]);
        for (version, hash) in [(2, 20), (3, 20), (4, 20), (2, 32), (4, 32)] {
            assert_eq!(
                tracked_paths(&index(version, hash, &[b"a"], Some(&link)), &mut ample()),
                Err(SPLIT_OR_SPARSE),
                "version {version}, {hash}-byte hashes"
            );
        }
    }

    #[test]
    fn r2_finding_6_only_uppercase_extensions_are_optional() {
        let extension = |signature: &[u8; 4]| {
            let mut e = signature.to_vec();
            e.extend_from_slice(&3u32.to_be_bytes());
            e.extend_from_slice(b"abc");
            e
        };
        for signature in [
            b"TREE", b"REUC", b"UNTR", b"FSMN", b"EOIE", b"IEOT", b"ZZZZ",
        ] {
            assert_eq!(
                tracked(&index(2, 20, &[b"a"], Some(&extension(signature)))),
                Some(set(&[b"a"])),
                "{}",
                String::from_utf8_lossy(signature)
            );
        }
        for signature in [b"link", b"sdir", b"fsmn", b"tree", b"\0abc", b"1abc"] {
            assert_eq!(
                tracked(&index(2, 20, &[b"a"], Some(&extension(signature)))),
                None,
                "{:?}",
                String::from_utf8_lossy(signature)
            );
        }
    }

    // -- review round 3, finding 2: the checksum decides the hash size ----------------

    /// The reviewer's fixture: 44 bytes, a version 2 header with no entries, a
    /// required extension with 4 payload bytes and a valid SHA-1 trailer. Read
    /// as SHA-256 the extension and the trailer's first bytes pass for a
    /// 32-byte trailer and the file looks like an empty index.
    #[test]
    fn r3_finding_2_a_required_extension_cannot_hide_in_a_sha256_trailer() {
        for signature in [b"aaaa", b"link", b"sdir"] {
            let mut extension = signature.to_vec();
            extension.extend_from_slice(&4u32.to_be_bytes());
            extension.extend_from_slice(&[0; 4]);
            let bytes = index(2, 20, &[], Some(&extension));
            let name = String::from_utf8_lossy(signature);

            assert_eq!(bytes.len(), 44, "{name}");
            assert_eq!(trailer(&bytes, 20), Trailer::Verified, "{name}");
            assert_eq!(trailer(&bytes, 32), Trailer::Mismatch, "{name}");
            assert_eq!(
                tracked_paths(&bytes, &mut ample()),
                Err(SPLIT_OR_SPARSE),
                "{name}"
            );
            // The same file with the optional signature is an empty index.
            let mut optional = extension.clone();
            optional[..4].copy_from_slice(b"AAAA");
            assert_eq!(
                tracked(&index(2, 20, &[], Some(&optional))),
                Some(set(&[])),
                "{name}"
            );
        }
        // The other way round: a SHA-256 index must not be read as SHA-1.
        let mut extension = b"link".to_vec();
        extension.extend_from_slice(&4u32.to_be_bytes());
        extension.extend_from_slice(&[0; 4]);
        for version in [2, 3, 4] {
            let bytes = index(version, 32, &[b"a"], Some(&extension));
            assert_eq!(trailer(&bytes, 32), Trailer::Verified);
            assert_eq!(
                tracked_paths(&bytes, &mut ample()),
                Err(SPLIT_OR_SPARSE),
                "version {version}"
            );
        }
    }

    #[test]
    fn r3_finding_2_an_index_whose_checksum_does_not_match_is_unknown() {
        for (version, hash) in [(2, 20), (3, 20), (4, 20), (2, 32), (4, 32)] {
            let paths: [&[u8]; 2] = [b"a", b"b"];
            let good = index(version, hash, &paths, None);
            assert_eq!(tracked(&good), Some(set(&paths)), "{version}/{hash}");

            // One flipped bit in an entry, then one in the trailer.
            for at in [20, good.len() - 1] {
                let mut bad = good.clone();
                bad[at] ^= 1;
                assert_eq!(
                    tracked_paths(&bad, &mut ample()),
                    Err(DAMAGED),
                    "version {version}, {hash}-byte hashes, byte {at}"
                );
            }
        }
    }

    #[test]
    fn r3_finding_2_a_skipped_checksum_is_accepted_when_it_fixes_the_hash_size() {
        // `index.skipHash` (set by `feature.manyFiles`): the trailer is zeros.
        let paths: [&[u8]; 2] = [b"a", b"src/b"];
        for (version, hash) in [(2, 20), (4, 20)] {
            let bytes = skip_hash(unsealed(version, hash, &paths, None), hash);
            assert_eq!(trailer(&bytes, 20), Trailer::Skipped);
            assert_eq!(trailer(&bytes, 32), Trailer::Mismatch);
            assert_eq!(tracked(&bytes), Some(set(&paths)), "version {version}");
        }
        // A skipped checksum still must not hide a required extension.
        let mut link = b"link".to_vec();
        link.extend_from_slice(&4u32.to_be_bytes());
        link.extend_from_slice(&[7; 4]);
        let bytes = skip_hash(unsealed(2, 20, &[b"a"], Some(&link)), 20);
        assert_eq!(tracked_paths(&bytes, &mut ample()), Err(SPLIT_OR_SPARSE));
    }

    /// Thirty-two zero bytes at the end are a skipped checksum of either size.
    /// Where one size reads the file and the other does not, Git would use
    /// whichever the repository's config says, and the scan does not read it.
    #[test]
    fn r3_finding_2_a_skipped_checksum_that_reads_differently_is_unknown() {
        // Header only, then 32 zeros: an empty index for SHA-256, but for SHA-1
        // the 12 bytes after the header are an extension that is not one.
        let mut bytes = b"DIRC".to_vec();
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&[0; 32]);
        assert_eq!(trailer(&bytes, 20), Trailer::Skipped);
        assert_eq!(trailer(&bytes, 32), Trailer::Skipped);
        assert_eq!(tracked_paths(&bytes, &mut ample()), Err(AMBIGUOUS));

        // A SHA-256 index written without a checksum: only its own layout parses.
        let sha256 = skip_hash(unsealed(2, 32, &[b"a"], None), 32);
        assert_eq!(tracked_paths(&sha256, &mut ample()), Err(AMBIGUOUS));
        // Neither reading works: damaged, not ambiguous.
        let mut garbage = bytes[..12].to_vec();
        garbage.extend_from_slice(b"TREE");
        garbage.extend_from_slice(&100u32.to_be_bytes());
        garbage.extend_from_slice(&[0; 32]);
        assert_eq!(trailer(&garbage, 20), Trailer::Skipped);
        assert_eq!(trailer(&garbage, 32), Trailer::Skipped);
        assert_eq!(tracked_paths(&garbage, &mut ample()), Err(DAMAGED));
        // The work of both readings is charged, and no more than the budget.
        let mut budget = 1000;
        assert_eq!(
            tracked_paths(&sha256, &mut budget),
            Err(AMBIGUOUS),
            "budget {budget}"
        );
        assert!(budget <= 1000);
    }
}
