//! Per-resource locks: one file per branch or snapshot, held by whoever is
//! changing it (a clone coordinator for the length of its operation, a command
//! for the length of a start or a drop).
//!
//! The kernel releases a lock when its holder dies, so a lock that is free
//! while the journal says an operation is unfinished means the coordinator is
//! gone, and recovery may begin. Several resources are always taken in
//! ascending id order, so two operations that need the same pair cannot wait
//! for each other.
use anyhow::{Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Held locks; released when dropped.
#[derive(Debug)]
pub struct Held {
    _files: Vec<File>,
}

fn lock_path(dir: &Path, service: &str, id: Uuid) -> PathBuf {
    dir.join(format!("{service}-{id}.lock"))
}

fn open(dir: &Path, service: &str, id: Uuid) -> Result<File> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(dir, service, id))
        .with_context(|| format!("cannot open the lock of {id}"))
}

/// Take the locks of `ids` (sorted and deduplicated here), waiting up to
/// `wait` in all. `None` when some other holder kept one that long; nothing
/// is held then.
pub fn acquire(dir: &Path, service: &str, ids: &[Uuid], wait: Duration) -> Result<Option<Held>> {
    let mut ids = ids.to_vec();
    ids.sort();
    ids.dedup();
    let start = Instant::now();
    loop {
        let mut files = Vec::with_capacity(ids.len());
        let mut blocked = false;
        for id in &ids {
            let file = open(dir, service, *id)?;
            match file.try_lock() {
                Ok(()) => files.push(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    blocked = true;
                    break;
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("cannot lock {id}"));
                }
            }
        }
        if !blocked {
            return Ok(Some(Held { _files: files }));
        }
        // Dropping `files` released what was taken: never wait while holding.
        drop(files);
        if start.elapsed() >= wait {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// A service key is `[a-z][a-z0-9_-]*`: it never has a dot, so `{service}.`
/// followed by anything is unambiguous, unlike `{service}-…` (the keys `a` and
/// `a-b` would share a prefix).
fn attempt_prefix(service: &str) -> String {
    format!("{service}.attempt.")
}

/// The file-name-safe form of an attempt's name.
fn attempt_stem(attempt: &str) -> String {
    attempt
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Take the lock in `dir/file_name`, waiting up to `wait`; `what` names it in
/// errors.
fn acquire_file(dir: &Path, file_name: &str, what: &str, wait: Duration) -> Result<Option<Held>> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(file_name))
        .with_context(|| format!("cannot open the lock of {what}"))?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(Held { _files: vec![file] })),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("cannot lock {what}"));
            }
        }
        if start.elapsed() >= wait {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// The lock of one helper attempt, by its name (unique to the attempt and
/// made of file-name-safe characters), waiting up to `wait`. It serialises
/// everything that looks at the attempt's container and changes the attempt's
/// record: a sweep's inspect, remove, fence and catalog write, a recovery's
/// removal of the helper, `forget`'s removal of the tombstone. `None` when
/// another holder kept it that long.
pub fn acquire_attempt(
    dir: &Path,
    service: &str,
    attempt: &str,
    wait: Duration,
) -> Result<Option<Held>> {
    acquire_file(
        dir,
        &format!("{}{}.lock", attempt_prefix(service), attempt_stem(attempt)),
        &format!("helper attempt {attempt}"),
        wait,
    )
}

/// The lock that makes `forget` exclusive per service: held from before the
/// `forgetting` marker is set until the entry is deleted (or the marker
/// cleared), so two forgets of one service never interleave. `None` when the
/// other kept it for `wait`.
pub fn acquire_forget(dir: &Path, service: &str, wait: Duration) -> Result<Option<Held>> {
    acquire_file(
        dir,
        &format!("{service}.forget.lock"),
        &format!("the forget of service {service}"),
        wait,
    )
}

/// Remove the lock files of service `service`'s helper attempts, except those
/// of `keep` (the attempts that are still listed). Returns how many went.
///
/// A lock file is never unlinked when it is released: a process waiting for it
/// has the old inode open, and a newcomer that creates the name again would
/// hold a different lock on the same attempt. Removal is safe only when
/// nothing can be waiting for an attempt that is still to be acted on, which
/// the caller (`forget`, inside its exclusive section) guarantees:
///
/// - the attempts that are listed are all in `keep`, so a file that is in use
///   for a listed attempt stays;
/// - an attempt that is not listed is resolved, and its name is never listed
///   again (names are unique; the only re-registration is by an operation's
///   coordinator, and `forget` holds every resource lock and refuses to
///   delete a service with an unfinished operation);
/// - a process that opened such a file earlier (a sweep that read the attempt
///   before it was resolved) reads the attempt again under the lock, finds it
///   unlisted and does nothing, whichever inode it holds.
///
/// The caller holds the catalog's lock, so no attempt is registered between
/// the list it passes and the removal.
pub fn prune_attempts(dir: &Path, service: &str, keep: &[String]) -> Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };
    let prefix = attempt_prefix(service);
    let keep: Vec<String> = keep.iter().map(|name| attempt_stem(name)).collect();
    let mut removed = 0;
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read {}", dir.display()))?;
        let file = entry.file_name();
        let Some(stem) = file
            .to_str()
            .and_then(|f| f.strip_prefix(&prefix))
            .and_then(|f| f.strip_suffix(".lock"))
        else {
            continue;
        };
        if keep.iter().any(|k| k == stem) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("cannot remove {}", entry.path().display()));
            }
        }
    }
    Ok(removed)
}

/// Whether any of `ids` is held by someone right now (without keeping it).
#[cfg(test)]
pub fn any_held(dir: &Path, service: &str, ids: &[Uuid]) -> Result<bool> {
    Ok(acquire(dir, service, ids, Duration::ZERO)?.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_resource_blocks_others_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let held = acquire(dir.path(), "cache", &[b, a], Duration::ZERO)
            .unwrap()
            .expect("free");
        assert!(any_held(dir.path(), "cache", &[a]).unwrap());
        assert!(any_held(dir.path(), "cache", &[b]).unwrap());
        assert!(
            acquire(dir.path(), "cache", &[a, b], Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
        // Another service key is another namespace.
        assert!(!any_held(dir.path(), "queue", &[a]).unwrap());
        drop(held);
        assert!(!any_held(dir.path(), "cache", &[a, b]).unwrap());
    }

    #[test]
    fn nothing_is_held_while_waiting_for_a_second_resource() {
        let dir = tempfile::tempdir().unwrap();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let held_b = acquire(dir.path(), "cache", &[b], Duration::ZERO)
            .unwrap()
            .unwrap();
        // Wants a and b; b is taken, so a must stay free for others.
        assert!(
            acquire(dir.path(), "cache", &[a, b], Duration::from_millis(80))
                .unwrap()
                .is_none()
        );
        assert!(!any_held(dir.path(), "cache", &[a]).unwrap());
        drop(held_b);
    }

    #[test]
    fn an_attempt_is_held_by_one_at_a_time_and_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let name = "devflow-0b3c9d2e-cache-copy-aaaaaaaaaaaa-bbbbbb";
        let held = acquire_attempt(dir.path(), "cache", name, Duration::ZERO)
            .unwrap()
            .expect("free");
        assert!(
            acquire_attempt(dir.path(), "cache", name, Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
        // Another attempt, and the same name of another service, are free.
        assert!(
            acquire_attempt(dir.path(), "cache", "other", Duration::ZERO)
                .unwrap()
                .is_some()
        );
        assert!(
            acquire_attempt(dir.path(), "queue", name, Duration::ZERO)
                .unwrap()
                .is_some()
        );
        // A waiter gets it as soon as the holder lets go.
        let path = dir.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            acquire_attempt(&path, "cache", name, Duration::from_secs(5))
                .unwrap()
                .is_some()
        });
        std::thread::sleep(Duration::from_millis(120));
        drop(held);
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn a_lock_is_released_when_its_holder_goes_away() {
        let dir = tempfile::tempdir().unwrap();
        let id = Uuid::from_u128(9);
        let waiter = {
            let path = dir.path().to_path_buf();
            let held = acquire(&path, "cache", &[id], Duration::ZERO)
                .unwrap()
                .unwrap();
            let handle = std::thread::spawn(move || {
                acquire(&path, "cache", &[id], Duration::from_secs(5))
                    .unwrap()
                    .is_some()
            });
            std::thread::sleep(Duration::from_millis(120));
            drop(held);
            handle
        };
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn forget_is_exclusive_per_service_and_released_with_its_holder() {
        let dir = tempfile::tempdir().unwrap();
        let held = acquire_forget(dir.path(), "cache", Duration::ZERO)
            .unwrap()
            .expect("free");
        assert!(
            acquire_forget(dir.path(), "cache", Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
        // Another service is free, and the forget lock is neither a resource
        // lock nor an attempt lock.
        assert!(
            acquire_forget(dir.path(), "queue", Duration::ZERO)
                .unwrap()
                .is_some()
        );
        assert!(!any_held(dir.path(), "cache", &[Uuid::from_u128(1)]).unwrap());
        assert!(
            acquire_attempt(dir.path(), "cache", "forget", Duration::ZERO)
                .unwrap()
                .is_some()
        );
        drop(held);
        assert!(
            acquire_forget(dir.path(), "cache", Duration::ZERO)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn pruning_removes_only_the_unlisted_attempt_locks_of_that_service() {
        let dir = tempfile::tempdir().unwrap();
        let names = |dir: &Path| {
            let mut files: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            files.sort();
            files
        };
        for (service, attempt) in [
            ("cache", "devflow-a-resolved"),
            ("cache", "devflow-a-listed"),
            // A service whose key starts with this one's must not be touched.
            ("cache-two", "devflow-b-resolved"),
            ("queue", "devflow-c-resolved"),
        ] {
            drop(acquire_attempt(dir.path(), service, attempt, Duration::ZERO).unwrap());
        }
        let resource = Uuid::from_u128(7);
        drop(acquire(dir.path(), "cache", &[resource], Duration::ZERO).unwrap());
        drop(acquire_forget(dir.path(), "cache", Duration::ZERO).unwrap());
        assert_eq!(names(dir.path()).len(), 6);

        let removed = prune_attempts(dir.path(), "cache", &["devflow-a-listed".into()]).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            names(dir.path()),
            [
                format!("cache-{resource}.lock"),
                "cache-two.attempt.devflow-b-resolved.lock".to_string(),
                "cache.attempt.devflow-a-listed.lock".to_string(),
                "cache.forget.lock".to_string(),
                "queue.attempt.devflow-c-resolved.lock".to_string(),
            ]
        );
        // Nothing to do twice, or without a directory.
        assert_eq!(
            prune_attempts(dir.path(), "cache", &["devflow-a-listed".into()]).unwrap(),
            0
        );
        assert_eq!(
            prune_attempts(&dir.path().join("none"), "cache", &[]).unwrap(),
            0
        );
    }
}
