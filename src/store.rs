//! StateStore — the durable-state boundary behind the contract.
//!
//! core's mutable state is reached only through this trait, never by addressing
//! the backend directly. [`FileStore`] is the only implementation today (it is
//! the current on-disk layout, behaviorally identical to the pre-trait code);
//! the trait is shaped from *operations* (read / atomic-write / exclusive-create
//! / remove / list), not paths, so an embedded-DB backend could implement it
//! without changing a single caller.
//!
//! Scope: this trait covers core's durable, contract-backed state — the mailbox
//! (`Ask`/`Answer`), the lease (`Claim`), goals, the PLAYBOOK, the journal, sensor
//! SCRIPTS, the goal-activity ledger, and the action write-ahead log. All of it is
//! reached only through these operations, so a DB backend could replace the file
//! layout wholesale.
//!
//! NOT in scope (deliberately, separate concerns):
//!   * CHANGE DETECTION — `worldhash` (the wake hash) and tick's `wait`
//!     fingerprints read policy files directly. Detecting "what changed" is
//!     inherently backend-specific (a DB would use a version column / NOTIFY),
//!     so it belongs to the backend, not to a generic consumer.
//!   * SensorRuntime — executing `sensors/*.sh` and the snapshots they emit. A
//!     sensor's CONTENT is state (here), but RUNNING it needs a real file to
//!     exec; that path stays on [`Paths`].
//!   * scratch / coordination — runs, prompts, the `.lock`,
//!     reports: regenerated / append-only / locking, different lifecycle.

use crate::paths::Paths;
use std::fs;
use std::io;

/// A logical, backend-agnostic address for one piece of durable state. A backend
/// maps each variant to its own storage (FileStore -> a path; a DB -> a row).
#[derive(Debug, Clone)]
pub enum Key {
    /// A worker's pending question (`looop ask`).
    Ask(String),
    /// The human's answer to an ask (`looop answer`).
    Answer(String),
    /// A worker's resource lease (`looop claim`).
    Claim(String),
    /// A goal spec (`goals/<id>.md`).
    Goal(String),
    /// The PLAYBOOK — the controller logic.
    Playbook,
    /// The action log (one line per executed move).
    Journal,
    /// A sensor SCRIPT (`sensors/<name>.sh`). Its content is state; executing it
    /// is SensorRuntime (not this trait).
    Sensor(String),
    /// The per-goal "last acted" ledger that drives `sys-goals` fairness.
    GoalActivity,
    /// Write-ahead intent log for the in-flight non-idempotent action.
    ActionWal,
    /// A durable time trigger (`schedules/<name>.json`) — one-shot or recurring.
    Schedule(String),
    /// A steering message for a running worker (`tells/<id>.json`).
    Tell(String),
}

/// A collection of keys to enumerate.
#[derive(Debug, Clone, Copy)]
pub enum Collection {
    Asks,
    Answers,
    Claims,
    Goals,
    Schedules,
    Tells,
}

impl Collection {
    /// The file extension the backing files carry (FileStore only).
    fn ext(self) -> &'static str {
        match self {
            Collection::Asks
            | Collection::Answers
            | Collection::Claims
            | Collection::Schedules
            | Collection::Tells => "json",
            Collection::Goals => "md",
        }
    }
}

/// The durable-state operations the contract verbs are built on. Every method is
/// expressible by both a filesystem and a DB; nothing returns a path.
pub trait StateStore {
    /// The stored contents of `key`, or `None` if absent.
    fn read(&self, key: &Key) -> Option<String>;

    /// Whether `key` currently exists.
    fn exists(&self, key: &Key) -> bool;

    /// Durably replace `key` with `contents`, atomically — a concurrent reader
    /// never observes a half-written value (FileStore: temp -> fsync -> rename).
    fn write_atomic(&self, key: &Key, contents: &str) -> io::Result<()>;

    /// Atomic create-if-absent — the mutual-exclusion primitive. Returns
    /// `Ok(true)` if this call created `key`, `Ok(false)` if it already existed.
    /// FileStore serializes writers with a per-directory lock and publishes via
    /// rename; a DB would use a unique insert. This is what lets two racers
    /// never both "win" a lease.
    fn create_exclusive(&self, key: &Key, contents: &str) -> io::Result<bool>;

    /// Append a line (with a trailing newline) to `key`, creating it if absent.
    /// Used for the journal / append-only logs.
    fn append_line(&self, key: &Key, line: &str) -> io::Result<()>;

    /// Move `key` into its archived form (FileStore: `goals/archive/<id>.md`).
    /// Only `Key::Goal` is archivable today.
    fn archive(&self, key: &Key) -> io::Result<()>;

    /// Remove `key`. Absent key is not an error (idempotent).
    fn remove(&self, key: &Key) -> io::Result<()>;

    /// COMPARE-AND-DELETE: remove `key` ONLY IF its current contents still
    /// equal `expected`. Returns `Ok(true)` when the key is now gone (we
    /// removed it, or it was already absent), `Ok(false)` when the contents
    /// changed underneath us and the key remains. This is what lets a stale-
    /// lease reclaim never delete a lease that was FRESHLY re-acquired between
    /// the caller's read and its delete (two racers can't both win: FileStore
    /// runs the read+compare+delete under the same per-directory writer lock
    /// create_exclusive takes, so the key is never observably ABSENT while a
    /// losing delete is in flight). A DB backend would use
    /// `DELETE … WHERE contents = ?`.
    fn remove_if_eq(&self, key: &Key, expected: &str) -> io::Result<bool>;

    /// Seconds since `key` was last written, or `None` when absent (or the
    /// backend cannot tell). Coarse — used only for staleness horizons (e.g.
    /// "is this empty claim file older than the in-flight grace period?").
    fn age_secs(&self, key: &Key) -> Option<u64>;

    /// The names present in `collection` (the `<name>` part of each key), in
    /// sorted order. For `Asks`/`Answers` that is the ask id; for `Claims` the
    /// claim name.
    fn list(&self, collection: &Collection) -> Vec<String>;
}

/// A per-directory WRITER lock: `flock(LOCK_EX)` on `<dir>/.dirlock`, released
/// when the guard drops (or the process dies — flock is kernel-managed, so a
/// crash never leaves a stale lock). Only the multi-step writer primitives
/// (`create_exclusive`, `remove_if_eq`) take it; READERS never do — they rely
/// on rename-published files, so "exists ⇒ contents complete" holds lock-free.
/// The `.dirlock` name has no extension, so `list`/`sorted_glob` (which filter
/// by `.json`/`.md`/…) never surface it as an entry.
struct DirLock {
    _file: fs::File,
}

impl DirLock {
    fn acquire(dir: &std::path::Path) -> io::Result<DirLock> {
        fs::create_dir_all(dir)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(".dirlock"))?;
        // Same libc-free extern-"C" flock technique as run.rs's single-instance
        // lock — blocking LOCK_EX here (writers queue; the critical sections
        // are a handful of syscalls).
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            const LOCK_EX: i32 = 2;
            unsafe extern "C" {
                fn flock(fd: i32, op: i32) -> i32;
            }
            if unsafe { flock(file.as_raw_fd(), LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(DirLock { _file: file })
    }
}

/// The filesystem-backed [`StateStore`] — the current on-disk layout. Borrows
/// the resolved [`Paths`] so it stays a thin mapping from logical key to file.
pub struct FileStore<'a> {
    paths: &'a Paths,
}

impl<'a> FileStore<'a> {
    pub fn new(paths: &'a Paths) -> Self {
        FileStore { paths }
    }

    /// Map a logical key to its backing file.
    fn path(&self, key: &Key) -> std::path::PathBuf {
        match key {
            Key::Ask(id) => self.paths.asks_dir().join(format!("{id}.json")),
            Key::Answer(id) => self.paths.answers_dir().join(format!("{id}.json")),
            Key::Claim(name) => self.paths.claims_dir().join(format!("{name}.json")),
            Key::Goal(id) => self.paths.goals_dir().join(format!("{id}.md")),
            Key::Playbook => self.paths.playbook(),
            Key::Journal => self.paths.journal(),
            Key::Sensor(name) => self.paths.sensors_dir().join(format!("{name}.sh")),
            Key::GoalActivity => self.paths.goal_activity(),
            Key::ActionWal => self.paths.action_wal(),
            Key::Schedule(name) => self.paths.schedules_dir().join(format!("{name}.json")),
            Key::Tell(id) => self.paths.tells_dir().join(format!("{id}.json")),
        }
    }

    /// Map a collection to its backing directory.
    fn dir(&self, c: &Collection) -> std::path::PathBuf {
        match c {
            Collection::Asks => self.paths.asks_dir(),
            Collection::Answers => self.paths.answers_dir(),
            Collection::Claims => self.paths.claims_dir(),
            Collection::Goals => self.paths.goals_dir(),
            Collection::Schedules => self.paths.schedules_dir(),
            Collection::Tells => self.paths.tells_dir(),
        }
    }
}

impl StateStore for FileStore<'_> {
    fn read(&self, key: &Key) -> Option<String> {
        fs::read_to_string(self.path(key)).ok()
    }

    fn exists(&self, key: &Key) -> bool {
        self.path(key).is_file()
    }

    fn write_atomic(&self, key: &Key, contents: &str) -> io::Result<()> {
        let path = self.path(key);
        // A sensor's content is a script the runtime execs, so the backing file
        // must be executable. The exec bit is set on the TEMP file BEFORE the
        // rename, so a concurrent exec never observes a non-executable window.
        let mode = if matches!(key, Key::Sensor(_)) {
            Some(0o755)
        } else {
            None
        };
        crate::util::write_atomic_mode(&path, contents.as_bytes(), mode)?;
        Ok(())
    }

    fn create_exclusive(&self, key: &Key, contents: &str) -> io::Result<bool> {
        let path = self.path(key);
        let parent = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // Mutual exclusion comes from the per-directory writer lock: while we
        // hold it, no other create_exclusive/remove_if_eq can interleave, so a
        // plain exists-check + rename-publish is race-free. rename (not
        // hard_link — which fails on SMB/NFS/FUSE mounts without link support)
        // keeps "exists ⇒ contents complete" for lock-free readers: the final
        // path only ever appears fully written + fsynced.
        let _lock = DirLock::acquire(&parent)?;
        if path.exists() {
            return Ok(false);
        }
        let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("key");
        let tmp = parent.join(format!(
            ".{stem}.{}.{}.excl.tmp",
            std::process::id(),
            crate::util::temp_nonce()
        ));
        let write = (|| -> io::Result<()> {
            use io::Write;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(contents.as_bytes())?;
            f.sync_all()
        })();
        if let Err(e) = write {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(true),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    fn append_line(&self, key: &Key, line: &str) -> io::Result<()> {
        use io::Write;
        let path = self.path(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{line}")
    }

    fn archive(&self, key: &Key) -> io::Result<()> {
        // Helper: move `from` into `<dir>/archive/`, suffixing on collision —
        // ask ids can be REUSED after a pair is archived (`next_ask_id` scans
        // only the live dirs), so an archived record must never be clobbered.
        fn into_archive(from: std::path::PathBuf, stem: &str, ext: &str) -> io::Result<()> {
            let dir = from
                .parent()
                .ok_or_else(|| io::Error::other("archive: no parent dir"))?
                .join("archive");
            fs::create_dir_all(&dir)?;
            let mut to = dir.join(format!("{stem}.{ext}"));
            let mut n = 1;
            while to.exists() {
                to = dir.join(format!("{stem}-{n}.{ext}"));
                n += 1;
            }
            fs::rename(&from, to)
        }
        match key {
            // A goal id can be recreated after archiving, so the archive must
            // suffix on collision like the mailbox does — a plain rename to a
            // fixed path would silently clobber the previous archived record.
            Key::Goal(id) => into_archive(self.path(key), id, "md"),
            // A consumed ask/answer pair (resumed detached ask) moves aside so
            // the sys-asks wake signal settles while the record stays auditable.
            Key::Ask(id) => into_archive(self.path(key), id, "json"),
            Key::Answer(id) => into_archive(self.path(key), id, "json"),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "archive: only goals and ask/answer records are archivable",
            )),
        }
    }

    fn remove(&self, key: &Key) -> io::Result<()> {
        match fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn remove_if_eq(&self, key: &Key, expected: &str) -> io::Result<bool> {
        let path = self.path(key);
        let parent = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // Read + compare + delete under the per-directory writer lock: no
        // rename-aside, so the key is NEVER observably absent while a losing
        // compare is in flight (the old design had a window where a concurrent
        // create_exclusive could win and then be destroyed by the restore).
        let _lock = DirLock::acquire(&parent)?;
        let actual = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(true), // already gone
            Err(e) => return Err(e),
        };
        if actual != expected {
            return Ok(false); // a FRESH value landed — the caller lost
        }
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e),
        }
    }

    fn age_secs(&self, key: &Key) -> Option<u64> {
        let modified = fs::metadata(self.path(key)).ok()?.modified().ok()?;
        // An mtime in the future (clock skew) reads as age 0, never an error.
        Some(modified.elapsed().map(|d| d.as_secs()).unwrap_or(0))
    }

    fn list(&self, collection: &Collection) -> Vec<String> {
        let ext = collection.ext();
        let mut names: Vec<String> = fs::read_dir(self.dir(collection))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == ext).unwrap_or(false))
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_remove_round_trip() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Ask("w-1".into());
        assert!(!s.exists(&k));
        s.write_atomic(&k, "hello").unwrap();
        assert!(s.exists(&k));
        assert_eq!(s.read(&k).as_deref(), Some("hello"));
        s.remove(&k).unwrap();
        assert!(!s.exists(&k));
        // Removing an absent key is a no-op success.
        s.remove(&k).unwrap();
    }

    #[test]
    fn create_exclusive_is_a_test_and_set() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Claim("repo".into());
        assert!(s.create_exclusive(&k, "first").unwrap(), "first wins");
        assert!(
            !s.create_exclusive(&k, "second").unwrap(),
            "second sees it already exists"
        );
        assert_eq!(s.read(&k).as_deref(), Some("first"), "loser never clobbers");
    }

    #[test]
    fn create_exclusive_never_exposes_an_empty_file() {
        // "Exists ⇒ contents complete": after Ok(true) the file is fully
        // written (the rename publish happens only after write+fsync), and
        // no temp siblings are left behind.
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Claim("lease".into());
        assert!(s.create_exclusive(&k, r#"{"session":"w1"}"#).unwrap());
        let body = s.read(&k).unwrap();
        assert!(!body.is_empty(), "created key must never read back empty");
        assert_eq!(body, r#"{"session":"w1"}"#);
        let leftovers: Vec<_> = fs::read_dir(p.claims_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp left behind: {leftovers:?}");
    }

    #[test]
    fn remove_if_eq_is_a_compare_and_delete() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Claim("repo".into());
        s.write_atomic(&k, "stale").unwrap();
        let observed = s.read(&k).unwrap();
        // A FRESH lease lands between the read and the reclaim …
        s.write_atomic(&k, "fresh").unwrap();
        // … so the compare-and-delete must LOSE and leave the fresh lease.
        assert!(!s.remove_if_eq(&k, &observed).unwrap());
        assert_eq!(s.read(&k).as_deref(), Some("fresh"), "fresh lease survives");
        // Matching contents: the delete wins and the key is gone.
        assert!(s.remove_if_eq(&k, "fresh").unwrap());
        assert!(!s.exists(&k));
        // Already-absent key: gone ⇒ Ok(true) (idempotent for reapers).
        assert!(s.remove_if_eq(&k, "anything").unwrap());
    }

    #[test]
    fn losing_remove_if_eq_leaves_no_temp_and_no_absent_window() {
        // Regression for the rename-aside design: a LOSING compare-and-delete
        // must never make the key observably ABSENT (which let a concurrent
        // create_exclusive slip in, only for the restore to destroy its fresh
        // lease) and must leave no .cad.tmp debris behind.
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Claim("repo".into());
        s.write_atomic(&k, "current").unwrap();
        std::thread::scope(|scope| {
            let intruder = scope.spawn(|| {
                let s2 = FileStore::new(&p);
                let k2 = Key::Claim("repo".into());
                let mut wins = 0;
                for _ in 0..200 {
                    if s2.create_exclusive(&k2, "intruder").unwrap() {
                        wins += 1;
                    }
                }
                wins
            });
            for _ in 0..200 {
                // Mismatched expected: every call must LOSE and change nothing.
                assert!(!s.remove_if_eq(&k, "mismatched").unwrap());
            }
            assert_eq!(
                intruder.join().unwrap(),
                0,
                "a losing compare-and-delete must never expose an absent window"
            );
        });
        assert_eq!(s.read(&k).as_deref(), Some("current"), "key untouched");
        let leftovers: Vec<_> = fs::read_dir(p.claims_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp left behind: {leftovers:?}");
    }

    #[test]
    fn age_secs_reports_presence_and_freshness() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Claim("repo".into());
        assert_eq!(s.age_secs(&k), None, "absent key has no age");
        s.write_atomic(&k, "body").unwrap();
        assert!(s.age_secs(&k).unwrap() < 5, "a fresh write is young");
    }

    #[test]
    fn dirlock_file_is_invisible_to_list() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        s.create_exclusive(&Key::Claim("a".into()), "{}").unwrap();
        assert!(
            p.claims_dir().join(".dirlock").is_file(),
            "the writer lock file exists after a locked write"
        );
        assert_eq!(
            s.list(&Collection::Claims),
            vec!["a"],
            ".dirlock must never surface as a claim"
        );
    }

    #[test]
    fn goal_archive_suffixes_instead_of_overwriting() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Goal("triage".into());
        s.write_atomic(&k, "first body").unwrap();
        s.archive(&k).unwrap();
        // The id is reused, then archived again — must NOT clobber the first.
        s.write_atomic(&k, "second body").unwrap();
        s.archive(&k).unwrap();
        let dir = p.goals_dir().join("archive");
        assert!(dir.join("triage.md").is_file());
        assert!(dir.join("triage-1.md").is_file());
        assert_eq!(
            fs::read_to_string(dir.join("triage.md")).unwrap(),
            "first body"
        );
        assert_eq!(
            fs::read_to_string(dir.join("triage-1.md")).unwrap(),
            "second body"
        );
    }

    #[test]
    fn list_returns_sorted_stems() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        s.write_atomic(&Key::Claim("b".into()), "{}").unwrap();
        s.write_atomic(&Key::Claim("a".into()), "{}").unwrap();
        assert_eq!(s.list(&Collection::Claims), vec!["a", "b"]);
    }
}
