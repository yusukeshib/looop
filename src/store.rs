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
}

/// A collection of keys to enumerate.
#[derive(Debug, Clone, Copy)]
pub enum Collection {
    Asks,
    Answers,
    Claims,
    Goals,
}

impl Collection {
    /// The file extension the backing files carry (FileStore only).
    fn ext(self) -> &'static str {
        match self {
            Collection::Asks | Collection::Answers | Collection::Claims => "json",
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
    /// FileStore uses `O_EXCL`; a DB would use a unique insert. This is what lets
    /// two racers never both "win" a lease.
    fn create_exclusive(&self, key: &Key, contents: &str) -> io::Result<bool>;

    /// Append a line (with a trailing newline) to `key`, creating it if absent.
    /// Used for the journal / append-only logs.
    fn append_line(&self, key: &Key, line: &str) -> io::Result<()>;

    /// Move `key` into its archived form (FileStore: `goals/archive/<id>.md`).
    /// Only `Key::Goal` is archivable today.
    fn archive(&self, key: &Key) -> io::Result<()>;

    /// Remove `key`. Absent key is not an error (idempotent).
    fn remove(&self, key: &Key) -> io::Result<()>;

    /// The names present in `collection` (the `<name>` part of each key), in
    /// sorted order. For `Asks`/`Answers` that is the ask id; for `Claims` the
    /// claim name.
    fn list(&self, collection: &Collection) -> Vec<String>;
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
        }
    }

    /// Map a collection to its backing directory.
    fn dir(&self, c: &Collection) -> std::path::PathBuf {
        match c {
            Collection::Asks => self.paths.asks_dir(),
            Collection::Answers => self.paths.answers_dir(),
            Collection::Claims => self.paths.claims_dir(),
            Collection::Goals => self.paths.goals_dir(),
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
        crate::util::write_atomic(&path, contents.as_bytes())?;
        // A sensor's content is a script the runtime execs, so the backing file
        // must be executable. The exec bit is a FileStore detail, not a caller's.
        #[cfg(unix)]
        if matches!(key, Key::Sensor(_)) {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(&path)?.permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&path, perm)?;
        }
        Ok(())
    }

    fn create_exclusive(&self, key: &Key, contents: &str) -> io::Result<bool> {
        let path = self.path(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use io::Write;
                f.write_all(contents.as_bytes())?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
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
        match key {
            Key::Goal(id) => {
                let from = self.paths.goals_dir().join(format!("{id}.md"));
                let archive = self.paths.goals_dir().join("archive");
                fs::create_dir_all(&archive)?;
                fs::rename(&from, archive.join(format!("{id}.md")))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "archive: only goals are archivable",
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
    fn list_returns_sorted_stems() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        s.write_atomic(&Key::Claim("b".into()), "{}").unwrap();
        s.write_atomic(&Key::Claim("a".into()), "{}").unwrap();
        assert_eq!(s.list(&Collection::Claims), vec!["a", "b"]);
    }
}
