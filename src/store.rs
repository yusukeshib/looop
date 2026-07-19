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
    /// Write-ahead intent record for ONE actor's in-flight non-idempotent
    /// action (`.action-wal.<actor>.json`; actor = pid-nonce, so concurrent
    /// actors can never clobber each other's crash guard).
    ActionWal(String),
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

    /// Whether `key` currently exists, distinguishing a definitive answer from
    /// a backend failure: `Ok(false)` ONLY on proven absence (NotFound); any
    /// other stat error (EACCES, EIO, …) is `Err`. The same discipline
    /// `create_exclusive` applies internally (fs::metadata, not
    /// `Path::exists()`), exposed so callers can tell "absent" from "could
    /// not look". There is deliberately NO error-squashing `exists()`
    /// convenience: a caller that makes a NEGATIVE decision from absence
    /// ("no pending ask", "the record vanished") would read a transient
    /// EACCES/EIO as "gone" and produce a wrong, destructive verdict.
    fn exists_checked(&self, key: &Key) -> io::Result<bool>;

    /// Durably replace `key` with `contents`, atomically — a concurrent reader
    /// never observes a half-written value (FileStore: temp -> fsync -> rename).
    ///
    /// INVARIANT: the replace is serialized against the OTHER writer
    /// primitives (`create_exclusive`, `remove_if_eq`) — FileStore publishes
    /// the rename under the same per-directory writer lock they take. Without
    /// this, `remove_if_eq`'s compare-and-delete guarantee would only hold
    /// against `create_exclusive` writers: a lock-free rename racing the
    /// locked read→compare→remove could land its FRESH value between the
    /// compare and the delete and have it destroyed.
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
    ///
    /// UNCONDITIONAL and LOCK-FREE — deliberately OUTSIDE the serialization
    /// contract of `write_atomic` / `create_exclusive` / `remove_if_eq`: it
    /// offers none of the compare-and-delete guarantees and may destroy a
    /// value published concurrently. Callers that decide "delete" from an
    /// earlier read must use [`StateStore::remove_if_eq`] instead.
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

    /// COMPARE-AND-SWAP: replace `key` with `contents` ONLY IF its current
    /// contents still equal `expected`. Returns `Ok(true)` when the swap
    /// happened, `Ok(false)` when the key is absent or its contents changed
    /// underneath us (the caller lost — re-inspect). The read + compare +
    /// rename-publish run under the same per-directory writer lock the other
    /// writer primitives take, so the key is NEVER observably absent during
    /// the swap — unlike a remove_if_eq + create_exclusive pair, which opens
    /// a window where a third racer's exclusive create wins against a value
    /// the caller was merely refreshing. A DB backend would use
    /// `UPDATE … SET contents = ? WHERE contents = ?`.
    fn replace_if_eq(&self, key: &Key, expected: &str, contents: &str) -> io::Result<bool>;

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
/// (`write_atomic`, `create_exclusive`, `remove_if_eq`) take it; READERS never do — they rely
/// on rename-published files, so "exists ⇒ contents complete" holds lock-free.
/// The `.dirlock` name has no extension, so `list`/`sorted_glob` (which filter
/// by `.json`/`.md`/…) never surface it as an entry. `pub(crate)` so the
/// mailbox's unarchive path can serialize its exists-check + rename against
/// the same writer lock `create_exclusive` takes (closing the TOCTOU where a
/// concurrent create of a reused ask id slips between check and rename).
pub(crate) struct DirLock {
    _file: fs::File,
}

impl DirLock {
    pub(crate) fn acquire(dir: &std::path::Path) -> io::Result<DirLock> {
        fs::create_dir_all(dir)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(".dirlock"))?;
        // Shared flock helper (util::flock_file) — blocking LOCK_EX here
        // (writers queue; the critical sections are a handful of syscalls).
        if !crate::util::flock_file(&file, true) {
            return Err(io::Error::last_os_error());
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
    ///
    /// CHOKE POINT for the id → file-name conversion: every verb validates its
    /// ids with `util::safe_segment` at the boundary, but path() is the single
    /// place an id actually BECOMES a path segment, so it re-checks here — N
    /// call-site checks can drift; this one cannot. The check is TRAVERSAL-only
    /// (empty, `/`, NUL — plus `\` where the host treats it as a separator),
    /// in debug and release alike: stems scanned
    /// back from disk by `list()` — and files a foreign tool dropped into a
    /// collection dir — may legally violate the HYGIENE-only rules (dots,
    /// whitespace, control chars), and those must stay readable so the reading
    /// layers can address/escape them (the prompt escapes exotic goal ids
    /// rather than pretending they don't exist). Full hygiene remains the
    /// verbs' boundary duty via `safe_segment`; but building a path that
    /// ESCAPES the collection directory is never acceptable, so that panics
    /// with a clear message instead of touching the filesystem outside the
    /// data dir.
    fn path(&self, key: &Key) -> std::path::PathBuf {
        fn checked<'i>(kind: &str, id: &'i str) -> &'i str {
            // NB: a bare `..` id is deliberately NOT rejected: every arm below
            // appends an extension (`{id}.json`/`.md`/`.sh`), so it lands as a
            // plain sibling file (`...json`), never a directory step — and
            // `list()` legally scans that stem back from a foreign `...json`
            // debris file, which must stay readable/addressable instead of
            // panicking the pulse into a crash loop every beat.
            //
            // `\` is rejected only where the HOST kernel treats it as a path
            // separator (Windows). On unix it is a legal file-name byte, and
            // `list()` scans it back as a stem from a foreign debris file
            // (`touch 'asks/foo\bar.json'`) — rejecting it here turned that
            // one unix-legal file into the same panic-per-beat crash loop the
            // `..` carve-out above defuses. Full hygiene (which does ban `\`
            // everywhere) remains the verbs' boundary duty via `safe_segment`;
            // this choke point only refuses what can actually ESCAPE.
            let escapes = |c: char| c == '/' || c == '\0' || (cfg!(windows) && c == '\\');
            if id.is_empty() || id.contains(escapes) {
                panic!("FileStore::path: {kind} {id:?} would escape its collection directory");
            }
            id
        }
        match key {
            Key::Ask(id) => self
                .paths
                .asks_dir()
                .join(format!("{}.json", checked("ask id", id))),
            Key::Answer(id) => self
                .paths
                .answers_dir()
                .join(format!("{}.json", checked("ask id", id))),
            Key::Claim(name) => self
                .paths
                .claims_dir()
                .join(format!("{}.json", checked("claim name", name))),
            Key::Goal(id) => self
                .paths
                .goals_dir()
                .join(format!("{}.md", checked("goal id", id))),
            Key::Playbook => self.paths.playbook(),
            Key::Journal => self.paths.journal(),
            Key::Sensor(name) => self
                .paths
                .sensors_dir()
                .join(format!("{}.sh", checked("sensor name", name))),
            Key::GoalActivity => self.paths.goal_activity(),
            Key::ActionWal(actor) => self.paths.action_wal(checked("wal actor", actor)),
            Key::Schedule(name) => self
                .paths
                .schedules_dir()
                .join(format!("{}.json", checked("schedule name", name))),
            Key::Tell(id) => self
                .paths
                .tells_dir()
                .join(format!("{}.json", checked("tell id", id))),
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

/// Best-effort ONE-GENERATION size rotation for an append-only log: when
/// `path` is past `max_bytes` (0 = capping off), rename it to `<name>.1`
/// (replacing any previous `.1`) so the live file stays bounded. Shared by
/// the journal ([`StateStore::append_line`]) and `events.jsonl`
/// (`events::emit`) — the discipline is identical in both and must never
/// drift: serialize ONLY the size-check + rename with a NON-blocking flock on
/// a sibling `.<name>.rotlock` file (kernel-managed — a crash never leaves a
/// stale lock; contended ⇒ skip, the next append past the cap retries), and
/// keep the append itself lock-free. All-ignore on purpose: rotation must
/// never fail (or even slow) a write.
///
/// KNOWN RACE (accepted): an appender that opened the live file just before
/// a concurrent rotation renames it keeps writing through its fd — that
/// record lands in the freshly-renamed `.1` generation instead of the new
/// live file. This is fine under the ONE-GENERATION policy: `.1` is retained
/// (readers that care scan live + `.1`), so the record survives exactly as
/// long as any other record of its generation; nothing is lost or torn.
/// Serializing appends against rotation to close it would put a lock on the
/// hot append path — the opposite of "rotation must never slow a write".
pub(crate) fn rotate_at_cap(path: &std::path::Path, max_bytes: u64) {
    let Some(name) = path.file_name().map(|s| s.to_string_lossy().to_string()) else {
        return;
    };
    if max_bytes > 0
        && let Ok(lock) = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_file_name(format!(".{name}.rotlock")))
        && crate::util::flock_file(&lock, false)
        && fs::metadata(path).is_ok_and(|m| m.len() > max_bytes)
    {
        let _ = fs::rename(path, path.with_file_name(format!("{name}.1")));
    }
}

impl StateStore for FileStore<'_> {
    fn read(&self, key: &Key) -> Option<String> {
        let path = self.path(key);
        match fs::read_to_string(&path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            // A non-NotFound error (EACCES, EIO, …) is NOT absence — callers
            // would misdiagnose it (claim() reads it as contention, pending()
            // silently skips the record). The Option return type stays (a
            // trait-wide Result refactor isn't worth it), but the real cause
            // must surface somewhere: one stderr line naming path and error.
            Err(e) => {
                eprintln!("looop: read {}: {e} — treating as absent", path.display());
                None
            }
        }
    }

    fn exists_checked(&self, key: &Key) -> io::Result<bool> {
        // fs::metadata, NOT Path::exists()/is_file(): those map EVERY stat
        // error (EACCES, EIO, …) to false — the same squash create_exclusive
        // documents — hiding failures from callers that must tell absence
        // from "could not look". Only a definitive NotFound is Ok(false).
        match fs::metadata(self.path(key)) {
            Ok(m) => Ok(m.is_file()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
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
        // Serialize the rename-publish against create_exclusive/remove_if_eq
        // (see the trait invariant): util::write_atomic_mode alone renames
        // WITHOUT the per-directory writer lock, which would let a fresh
        // value land between remove_if_eq's compare and its delete — and be
        // deleted. Taking the DirLock here restores the compare-and-delete
        // guarantee for ALL writers, not just exclusive-create ones.
        let parent = path.parent().map_or_else(
            || std::path::PathBuf::from("."),
            std::path::Path::to_path_buf,
        );
        let _lock = DirLock::acquire(&parent)?;
        crate::util::write_atomic_mode(&path, contents.as_bytes(), mode)?;
        Ok(())
    }

    fn create_exclusive(&self, key: &Key, contents: &str) -> io::Result<bool> {
        let path = self.path(key);
        let parent = path.parent().map_or_else(
            || std::path::PathBuf::from("."),
            std::path::Path::to_path_buf,
        );
        // Mutual exclusion comes from the per-directory writer lock: while we
        // hold it, no other create_exclusive/remove_if_eq can interleave, so a
        // plain exists-check + rename-publish is race-free. rename (not
        // hard_link — which fails on SMB/NFS/FUSE mounts without link support)
        // keeps "exists ⇒ contents complete" for lock-free readers: the final
        // path only ever appears fully written + fsynced.
        let _lock = DirLock::acquire(&parent)?;
        // fs::metadata, NOT path.exists(): exists() maps EVERY stat error
        // (EACCES, EIO, …) to false, which would let the rename below CLOBBER
        // a record we merely failed to see — breaking lease exclusivity. Only
        // a definitive NotFound may proceed; any other error surfaces.
        match fs::metadata(&path) {
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
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
            // Same mode selection as write_atomic/replace_if_eq: a sensor's
            // content is a script the runtime execs, so the exec bit goes on
            // the TEMP file BEFORE the rename (a concurrent exec never sees a
            // non-executable window) and BEFORE sync_all (the fsync covers
            // the permission metadata too). Omitting it here silently shipped
            // a non-executable sensor whenever the exclusive-create path won.
            #[cfg(unix)]
            if matches!(key, Key::Sensor(_)) {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(fs::Permissions::from_mode(0o755))?;
            }
            f.sync_all()
        })();
        if let Err(e) = write {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        match fs::rename(&tmp, &path) {
            Ok(()) => {
                // Durability of the rename itself (matches write_atomic_mode):
                // fsync the parent dir so the new entry survives a crash —
                // otherwise a claim/ask reported as created could vanish on
                // power loss (double-lease risk). Best-effort: the contents
                // are already synced, and durability here is defense in depth.
                let _ = crate::util::sync_parent_dir(&path);
                Ok(true)
            }
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
        // journal.md gets the same one-generation size cap events.jsonl has
        // (`LOOOP_JOURNAL_MAX_BYTES`, default 5 MiB, 0 = off): the journal is
        // append-only with no other pruning, so without a cap it grows without
        // bound. The mechanics (non-blocking flock + rename, best-effort) are
        // shared with events.rs in [`rotate_at_cap`].
        if matches!(key, Key::Journal) {
            let max_bytes: u64 =
                crate::util::env_knob("LOOOP_JOURNAL_MAX_BYTES").unwrap_or(5 * 1024 * 1024);
            rotate_at_cap(&path, max_bytes);
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        // ONE line per record is the append-log's integrity invariant (the
        // journal is parsed line-by-line; a record with an embedded newline
        // would forge extra entries). executor::append_journal already
        // collapses LLM-provided newlines, but this is the choke point every
        // appender funnels through — defend here too so no future caller can
        // reintroduce the hole. Flatten to spaces (never error): a journal
        // write must not fail a move over cosmetic whitespace.
        let line = if line.contains('\n') || line.contains('\r') {
            std::borrow::Cow::Owned(line.replace(['\r', '\n'], " "))
        } else {
            std::borrow::Cow::Borrowed(line)
        };
        // One write(2) for the whole line (mirrors events.rs): `writeln!` can
        // issue the line and the trailing newline as SEPARATE writes, letting
        // a concurrent appender (pulse + CLI journal writes race) interleave
        // mid-line. O_APPEND + a single write_all keeps each line intact.
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(&line);
        buf.push('\n');
        f.write_all(buf.as_bytes())
    }

    fn archive(&self, key: &Key) -> io::Result<()> {
        // Helper: move `from` into `<dir>/archive/`, suffixing on collision —
        // ask ids can be REUSED after a pair is archived (`next_ask_id` scans
        // only the live dirs), so an archived record must never be clobbered.
        fn into_archive(from: std::path::PathBuf, stem: &str, ext: &str) -> io::Result<()> {
            let live_dir = from
                .parent()
                .ok_or_else(|| io::Error::other("archive: no parent dir"))?;
            let dir = live_dir.join("archive");
            fs::create_dir_all(&dir)?;
            // The free-suffix scan + rename must be ONE critical section:
            // unlocked, two concurrent archives of the same id could both pick
            // the same free suffix and the second rename would silently
            // clobber the first record — and unarchive_pair's no-clobber guard
            // already assumes archive-side movement is serialized under the
            // LIVE dir's writer lock (the same DirLock create_exclusive
            // takes), so that lock is the shared serialization point.
            let _lock = DirLock::acquire(live_dir)?;
            let mut to = dir.join(format!("{stem}.{ext}"));
            let mut n = 1;
            loop {
                match fs::metadata(&to) {
                    // fs::metadata, NOT to.exists(): exists() maps every stat
                    // error (EACCES, EIO, …) to "absent", which would let the
                    // rename below OVERWRITE an archived record we merely
                    // failed to see. Only a definitive NotFound frees a slot.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                    // Occupied — probe the next suffix.
                    Ok(_) => {}
                    // A stat failure would repeat for EVERY probe (the whole
                    // dir is unreadable, not one slot) — surface it instead of
                    // scanning forever or treating a slot as free.
                    Err(e) => return Err(e),
                }
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

    /// UNCONDITIONAL remove — LOCK-FREE, and deliberately OUTSIDE the
    /// write_atomic / create_exclusive / remove_if_eq serialization contract:
    /// it takes no DirLock, so it provides NONE of the compare-and-delete
    /// guarantees. A remove() racing a concurrent writer can delete a value
    /// it never inspected (including one freshly published). Use it only
    /// where destroying ANY current value is the intent (discard_tells, test
    /// teardown); anything that decides "delete" from an earlier read must go
    /// through remove_if_eq instead.
    fn remove(&self, key: &Key) -> io::Result<()> {
        match fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn remove_if_eq(&self, key: &Key, expected: &str) -> io::Result<bool> {
        let path = self.path(key);
        let parent = path.parent().map_or_else(
            || std::path::PathBuf::from("."),
            std::path::Path::to_path_buf,
        );
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

    fn replace_if_eq(&self, key: &Key, expected: &str, contents: &str) -> io::Result<bool> {
        let path = self.path(key);
        let parent = path.parent().map_or_else(
            || std::path::PathBuf::from("."),
            std::path::Path::to_path_buf,
        );
        // Read + compare + rename-publish under the per-directory writer lock
        // (same discipline as remove_if_eq): the swap is atomic against every
        // other locked writer, and lock-free readers only ever observe the
        // old complete value or the new complete value — never absence.
        let _lock = DirLock::acquire(&parent)?;
        match fs::read_to_string(&path) {
            Ok(s) if s == expected => {}
            Ok(_) => return Ok(false), // a FRESH value landed — the caller lost
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false), // vanished
            Err(e) => return Err(e),
        }
        // util::write_atomic_mode directly (NOT self.write_atomic): that
        // method acquires the same DirLock and flock does not nest across
        // fds within one process — calling it here would deadlock. Same mode
        // selection as write_atomic, though: a sensor's content is a script
        // the runtime execs, so a hardcoded None here would silently strip
        // the exec bit if a compare-and-swap is ever used for Key::Sensor.
        let mode = if matches!(key, Key::Sensor(_)) {
            Some(0o755)
        } else {
            None
        };
        crate::util::write_atomic_mode(&path, contents.as_bytes(), mode)?;
        Ok(true)
    }

    fn age_secs(&self, key: &Key) -> Option<u64> {
        let modified = fs::metadata(self.path(key)).ok()?.modified().ok()?;
        // An mtime in the future (clock skew) reads as age 0, never an error.
        Some(modified.elapsed().map_or(0, |d| d.as_secs()))
    }

    fn list(&self, collection: &Collection) -> Vec<String> {
        let ext = collection.ext();
        let dir = self.dir(collection);
        let entries = match fs::read_dir(&dir) {
            Ok(it) => it,
            // A collection dir that was never created IS an empty collection.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
            // A non-NotFound error (EACCES, EIO, …) is NOT emptiness — an
            // "empty" answer here falsely settles wake signals (sys_asks,
            // sys_claims) built on these listings. The Vec return type stays
            // (mirrors read()'s Option — a trait-wide Result refactor isn't
            // worth it), but the real cause must surface: one stderr line
            // naming directory and error, same discipline as read().
            Err(e) => {
                eprintln!("looop: list {}: {e} — treating as empty", dir.display());
                return Vec::new();
            }
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == ext))
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .collect();
        names.sort();
        names
    }
}

/// Restores an env var to its pre-test value on drop (panic-safe). Shared
/// test helper: env-mutating tests in mailbox.rs and paths.rs import it from
/// here instead of each keeping a drifting copy (`util::test_env_lock` — the
/// guard every user of this must also hold — lives in util, but util keeps
/// its module surface minimal, so the struct lives next to the store tests
/// that first needed it).
#[cfg(test)]
pub(crate) struct EnvRestore(&'static str, Option<std::ffi::OsString>);
#[cfg(test)]
impl EnvRestore {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        EnvRestore(key, prev)
    }
}
#[cfg(test)]
impl Drop for EnvRestore {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => unsafe { std::env::set_var(self.0, v) },
            None => unsafe { std::env::remove_var(self.0) },
        }
    }
}

/// Restores a directory's mode to 0o755 on drop — the teardown half of
/// [`deny_access`], so a panicking assert never strands an unreadable temp
/// dir (which would break the temp-dir cleanup).
#[cfg(all(test, unix))]
pub(crate) struct AccessRestore(std::path::PathBuf);
#[cfg(all(test, unix))]
impl Drop for AccessRestore {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

/// Make `dir` unsearchable (mode 000) and report whether the kernel actually
/// enforces it — root (some CI containers) bypasses permission checks, in
/// which case the caller should skip its assertions. Returns the guard that
/// restores the mode on drop. Shared test helper (mailbox.rs's error-path
/// tests import it), kept next to [`EnvRestore`] for the same
/// no-drifting-copies reason.
#[cfg(all(test, unix))]
pub(crate) fn deny_access(dir: std::path::PathBuf) -> (bool, AccessRestore) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    let enforced = fs::read_dir(&dir).is_err();
    (enforced, AccessRestore(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_remove_round_trip() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Ask("w-1".into());
        assert!(!s.exists_checked(&k).unwrap());
        s.write_atomic(&k, "hello").unwrap();
        assert!(s.exists_checked(&k).unwrap());
        assert_eq!(s.read(&k).as_deref(), Some("hello"));
        s.remove(&k).unwrap();
        assert!(!s.exists_checked(&k).unwrap());
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
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp left behind: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn create_exclusive_publishes_a_sensor_with_the_exec_bit() {
        // Regression: create_exclusive skipped the Key::Sensor → 0o755 mode
        // selection write_atomic/replace_if_eq apply — a sensor published
        // through the exclusive-create path landed non-executable and every
        // subsequent beat's exec of it failed with EACCES.
        use std::os::unix::fs::PermissionsExt;
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Sensor("probe".into());
        assert!(s.create_exclusive(&k, "#!/bin/sh\necho '{}'\n").unwrap());
        let mode = fs::metadata(p.sensors_dir().join("probe.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "sensor script must be executable");
        // A non-sensor key stays at the default (no exec bit smeared on it).
        let k2 = Key::Claim("lease".into());
        assert!(s.create_exclusive(&k2, "{}").unwrap());
        let mode2 = fs::metadata(p.claims_dir().join("lease.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode2 & 0o111, 0, "a claim must not become executable");
    }

    #[test]
    fn append_line_flattens_embedded_newlines_to_one_record() {
        // One line per record is the journal's integrity invariant — an
        // embedded newline would forge extra entries for a line-by-line
        // parser. The store is the choke point every appender funnels
        // through, so it flattens instead of trusting each caller.
        let p = Paths::temp();
        let s = FileStore::new(&p);
        s.append_line(&Key::Journal, "first\nsecond\r\nthird").unwrap();
        s.append_line(&Key::Journal, "clean").unwrap();
        let body = fs::read_to_string(p.journal()).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines,
            vec!["first second  third", "clean"],
            "embedded newlines collapse to spaces — exactly one line per append"
        );
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
        assert!(!s.exists_checked(&k).unwrap());
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
            .filter_map(std::result::Result::ok)
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
    fn concurrent_archives_never_clobber_an_archived_record() {
        // Regression for the unlocked suffix scan: two concurrent archives of
        // the SAME id could both pick the same free `-N` suffix, and the
        // loser's rename silently destroyed the winner's archived record.
        // Under the live-dir DirLock every successful archive must land under
        // its OWN name, so files-in-archive == successful-archive-calls.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let p = Paths::temp();
        let archived = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    let s = FileStore::new(&p);
                    let k = Key::Goal("hot".into());
                    for i in 0..40 {
                        // The write may be overwritten (or archived away) by
                        // the sibling thread — only Ok archives are counted.
                        let _ = s.write_atomic(&k, &format!("body {i}"));
                        if s.archive(&k).is_ok() {
                            archived.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let files = fs::read_dir(p.goals_dir().join("archive"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
            .count();
        assert_eq!(
            files,
            archived.load(Ordering::Relaxed),
            "every successful archive must keep its own record (no suffix clobber)"
        );
    }

    #[test]
    fn journal_rotates_one_generation_at_the_size_cap() {
        // set_var is process-global: serialize against other env-mutating
        // tests and restore the knob even if an assert panics.
        let _g = crate::util::test_env_lock();
        let _r = EnvRestore::set("LOOOP_JOURNAL_MAX_BYTES", "64");
        let p = Paths::temp();
        let s = FileStore::new(&p);
        for i in 0..8 {
            s.append_line(&Key::Journal, &format!("- entry {i} xxxxxxxxxxxxxxxx"))
                .unwrap();
        }
        let rotated = p.data_dir.join("journal.md.1");
        assert!(
            rotated.is_file(),
            "past the cap the journal must roll to journal.md.1"
        );
        let live = fs::read_to_string(p.journal()).unwrap();
        assert!(
            live.len() <= 64 + 32,
            "the live journal stays bounded near the cap, got {} bytes",
            live.len()
        );
        // One-generation policy (matches events.jsonl): the newest entries are
        // split across live + .1; anything older was deliberately dropped when
        // a later rotation replaced the previous .1 — so assert recency, not
        // full retention.
        let all = format!("{}{live}", fs::read_to_string(&rotated).unwrap());
        assert!(
            all.contains("- entry 7 "),
            "the newest entry must survive rotation"
        );
        assert!(
            !all.contains("- entry 0 "),
            "the oldest generation is dropped (one rotated generation kept)"
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

    #[test]
    #[cfg(unix)]
    fn exists_checked_distinguishes_absence_from_stat_failure() {
        // Regression: exists() squashes EVERY stat error to "absent", which
        // let callers turn a transient EACCES/EIO into a destructive verdict
        // ("no pending ask", "the ask record vanished"). exists_checked must
        // return Ok(false) ONLY on proven absence and Err otherwise.
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Claim("repo".into());
        assert!(matches!(s.exists_checked(&k), Ok(false)), "proven absence");
        s.write_atomic(&k, "{}").unwrap();
        assert!(matches!(s.exists_checked(&k), Ok(true)), "proven presence");
        let (enforced, _restore) = deny_access(p.claims_dir());
        if !enforced {
            return; // running as root — permissions can't simulate EACCES
        }
        assert!(
            s.exists_checked(&k).is_err(),
            "a stat failure must surface as Err, never as absence"
        );
    }

    #[test]
    #[cfg(unix)]
    fn list_on_an_unreadable_dir_returns_empty_without_panicking() {
        // The Vec-returning signature can't carry the error — the fix is the
        // stderr warning (not assertable here) plus NOT crashing the beat.
        // This pins the fallback behavior: unreadable ⇒ empty, and a restored
        // dir lists normally again.
        let p = Paths::temp();
        let s = FileStore::new(&p);
        s.write_atomic(&Key::Claim("a".into()), "{}").unwrap();
        {
            let (enforced, _restore) = deny_access(p.claims_dir());
            if !enforced {
                return; // running as root — permissions can't simulate EACCES
            }
            assert!(s.list(&Collection::Claims).is_empty());
        }
        assert_eq!(s.list(&Collection::Claims), vec!["a"], "recovers after");
    }

    #[test]
    #[cfg(unix)]
    fn archive_never_overwrites_when_the_suffix_probe_cannot_stat() {
        // Regression: the suffix probe used `to.exists()`, so an EACCES on the
        // archive dir read as "slot free" and the rename OVERWROTE an existing
        // archived record. A stat failure must abort the archive (Err) and
        // leave both the live and the archived record intact.
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let k = Key::Goal("triage".into());
        s.write_atomic(&k, "first body").unwrap();
        s.archive(&k).unwrap();
        s.write_atomic(&k, "second body").unwrap();
        let arch = p.goals_dir().join("archive");
        let (enforced, _restore) = deny_access(arch.clone());
        if !enforced {
            return; // running as root — permissions can't simulate EACCES
        }
        assert!(
            s.archive(&k).is_err(),
            "an unstattable archive dir must fail the archive, not clobber"
        );
        assert_eq!(
            s.read(&k).as_deref(),
            Some("second body"),
            "the live record stays put on a failed archive"
        );
        drop(_restore);
        assert_eq!(
            fs::read_to_string(arch.join("triage.md")).unwrap(),
            "first body",
            "the previously archived record was never overwritten"
        );
    }

    #[test]
    fn dotdot_debris_stays_addressable_without_a_panic() {
        // Regression: `list()` scans a foreign `...json` file back as the
        // stem `..`, and readers feed that straight into `Key::Ask` — the
        // choke point rejecting a bare `..` turned one debris file into a
        // panic-per-beat crash loop. Extension suffixing defuses it (the key
        // lands on the sibling `...json`, never a directory step), so it must
        // read fine.
        let p = Paths::temp();
        let s = FileStore::new(&p);
        std::fs::create_dir_all(p.asks_dir()).unwrap();
        std::fs::write(p.asks_dir().join("...json"), "{}").unwrap();
        assert_eq!(s.list(&Collection::Asks), vec![".."]);
        assert_eq!(s.read(&Key::Ask("..".into())).as_deref(), Some("{}"));
    }

    #[cfg(unix)]
    #[test]
    fn backslash_debris_stays_addressable_without_a_panic() {
        // Regression: `\` is a legal file-name byte on unix, and `list()`
        // scans a foreign `foo\bar.json` file back as the stem `foo\bar` —
        // the choke point rejecting `\` unconditionally turned that one
        // debris file (e.g. `touch 'asks/foo\bar.json'`) into a
        // panic-per-beat crash loop, the exact failure mode the `..` test
        // above defuses. `\` is not a separator here, so the key must stay
        // readable; the rejection is Windows-only.
        let p = Paths::temp();
        let s = FileStore::new(&p);
        std::fs::create_dir_all(p.asks_dir()).unwrap();
        std::fs::write(p.asks_dir().join("foo\\bar.json"), "{}").unwrap();
        assert_eq!(s.list(&Collection::Asks), vec!["foo\\bar"]);
        assert_eq!(s.read(&Key::Ask("foo\\bar".into())).as_deref(), Some("{}"));
    }

    #[test]
    fn replace_if_eq_is_a_compare_and_swap() {
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let key = Key::Claim("cas".into());
        // Absent key: nothing to swap.
        assert!(!s.replace_if_eq(&key, "old", "new").unwrap());
        assert!(
            s.read(&key).is_none(),
            "a losing swap must not create the key"
        );
        // Matched expectation: the swap lands.
        s.write_atomic(&key, "old").unwrap();
        assert!(s.replace_if_eq(&key, "old", "new").unwrap());
        assert_eq!(s.read(&key).as_deref(), Some("new"));
        // Stale expectation: the swap loses and the fresh value survives —
        // this is what lets the gate refresh a lease it owns without ever
        // clobbering one a third racer published meanwhile.
        assert!(!s.replace_if_eq(&key, "old", "stomp").unwrap());
        assert_eq!(s.read(&key).as_deref(), Some("new"));
    }

    #[test]
    #[should_panic(expected = "ask id")]
    fn path_is_a_choke_point_against_traversal_ids() {
        // Regression: path() used to trust its N call sites to have run
        // safe_segment; a single missed check let an id become a traversal-
        // capable path. The choke point must refuse `..`/`/` ids even when a
        // caller skips validation.
        let p = Paths::temp();
        let s = FileStore::new(&p);
        let _ = s.read(&Key::Ask("../evil".into()));
    }
}
