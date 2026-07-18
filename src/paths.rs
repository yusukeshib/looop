//! Path + profile layer — a faithful port of the bash header's path block.
//!
//! CODE / CONFIG / DATA are cleanly separated and all overridable by env:
//!   DATA    = $LOOOP_DATA_DIR        or ${XDG_STATE_HOME:-~/.local/state}/looop
//!   CONFIG  = $LOOOP_CONFIG          or <DATA>/config.json (per-profile, M5)
//!
//! We intentionally do NOT use the `directories` crate: it maps XDG dirs to
//! ~/Library/Application Support on macOS, which would diverge from the bash
//! version's ~/.local/state. Replicate the shell's plain XDG-with-HOME-fallback.

use std::env;
use std::path::PathBuf;

/// Everything the rest of the program needs to locate state.
pub struct Paths {
    /// The looop binary's own absolute path (exported to workers as $LOOOP_BIN).
    pub bin: PathBuf,
    /// The file-based memory dir ($LOOOP_DATA_DIR).
    pub data_dir: PathBuf,
    /// The single runner-wiring config file ($LOOOP_CONFIG).
    pub config: PathBuf,
    /// True when this is the default profile (data_dir == the XDG default), used
    /// only to decide whether shell hints need an explicit `LOOOP_DATA_DIR=`.
    pub default_profile: bool,
    /// Test-only: delete `data_dir` on drop. Set ONLY by [`Paths::temp`] — a
    /// `resolve()`d Paths points at REAL state and must never be auto-deleted.
    #[cfg(test)]
    pub(crate) temp_cleanup: bool,
}

fn home() -> PathBuf {
    match env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        // A clean, actionable exit instead of a panic + backtrace: $HOME being
        // unset is an environment problem (cron / stripped-down service env),
        // not a bug worth a panic message.
        //
        // EMBEDDING CAVEAT: process::exit in a library-layer fn is a CLI UX
        // choice — looop is a binary, so exiting here is the whole program's
        // answer. Anyone lifting this module into a library context should
        // turn this arm into an Err instead: exit(2) would tear down the HOST
        // process (skipping its destructors) on a missing env var.
        _ => {
            eprintln!(
                "looop: $HOME is not set — set HOME, or point LOOOP_DATA_DIR (and \
                 XDG_STATE_HOME) at explicit paths"
            );
            std::process::exit(2);
        }
    }
}

/// `${XDG_<name>:-$HOME/<fallback>}` — env override else HOME-relative default.
fn xdg(var: &str, fallback: &str) -> PathBuf {
    match env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

impl Paths {
    pub fn resolve() -> Self {
        let bin = env::current_exe().unwrap_or_else(|_| PathBuf::from("looop"));

        let default_data = xdg("XDG_STATE_HOME", ".local/state").join("looop");
        let data_dir = match env::var_os("LOOOP_DATA_DIR") {
            // Absolutize a relative LOOOP_DATA_DIR against the cwd ONCE, here:
            // the pulse, workers, and CLI invocations all run from different
            // directories, so a relative value left as-is would silently give
            // each process its OWN profile depending on cwd — the loop's state
            // would fragment invisibly. `std::path::absolute` (no symlink
            // resolution, no fs access) is enough; if even that fails (cwd
            // gone), keep the raw value rather than crash path resolution.
            Some(v) if !v.is_empty() => {
                let p = PathBuf::from(v);
                std::path::absolute(&p).unwrap_or(p)
            }
            _ => default_data.clone(),
        };

        // Config lives INSIDE the data dir so a profile is fully self-contained
        // (copy the dir = copy its runner wiring) and splitting LOOOP_DATA_DIR
        // also splits the config — fixes M5 (config was profile-global). An
        // explicit $LOOOP_CONFIG still wins for sharing one wiring across
        // profiles.
        let config = match env::var_os("LOOOP_CONFIG") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => data_dir.join("config.json"),
        };

        // Worker-fleet isolation: the session store ALWAYS lives inside this
        // profile's data dir (`<data_dir>/sessions`), derived purely from
        // LOOOP_DATA_DIR (ignoring any inherited BABYSIT_DIR). Every profile —
        // including the default one — is therefore self-contained, so session
        // ids never need a `looop-` prefix to be disambiguated from anything
        // else in a shared root.
        let default_profile = data_dir == default_data;

        Paths {
            bin,
            data_dir,
            config,
            default_profile,
            #[cfg(test)]
            temp_cleanup: false,
        }
    }

    /// This profile's session store, as an explicit context. The state root is
    /// the data dir itself, so sessions live at `<LOOOP_DATA_DIR>/sessions/<id>`
    /// — self-contained per profile, configured by an explicit path rather than
    /// any ambient environment. (The library nests sessions under
    /// `<root>/sessions/`, so the root is the data dir, not a `sessions` subdir.)
    pub fn sessions(&self) -> ::babysit::Babysit {
        ::babysit::Babysit::new(&self.data_dir)
    }

    // ---- derived data-dir paths (mirror the bash globals) -------------------
    pub fn sensors_dir(&self) -> PathBuf {
        self.data_dir.join("sensors")
    }
    pub fn playbook(&self) -> PathBuf {
        self.data_dir.join("PLAYBOOK.md")
    }
    pub fn goals_dir(&self) -> PathBuf {
        self.data_dir.join("goals")
    }
    pub fn journal(&self) -> PathBuf {
        self.data_dir.join("journal.md")
    }
    pub fn lock(&self) -> PathBuf {
        self.data_dir.join(".lock")
    }
    pub fn snapshots_dir(&self) -> PathBuf {
        self.data_dir.join("snapshots")
    }
    pub fn runs_dir(&self) -> PathBuf {
        self.data_dir.join("runs")
    }
    pub fn claims_dir(&self) -> PathBuf {
        self.data_dir.join("claims")
    }
    pub fn reports_dir(&self) -> PathBuf {
        self.data_dir.join("reports")
    }
    /// Mailbox: questions a worker raises for the human (`looop ask`).
    /// One JSON file per ask (`asks/<worker>-<n>.json`); a matching
    /// `answers/<worker>-<n>.json` resolves it. Durable + level-triggered — a
    /// crashed pulse re-reads the unanswered asks on restart.
    pub fn asks_dir(&self) -> PathBuf {
        self.data_dir.join("asks")
    }
    /// Mailbox: answers the human writes back (`looop answer`).
    pub fn answers_dir(&self) -> PathBuf {
        self.data_dir.join("answers")
    }
    pub fn prompts_dir(&self) -> PathBuf {
        self.data_dir.join("prompts")
    }
    /// Per-goal "last acted" ledger (goal id -> RFC3339 ts). Drives the
    /// `sys-goals` staleness reading so the decider can avoid starving a goal.
    pub fn goal_activity(&self) -> PathBuf {
        self.data_dir.join(".goal-activity.json")
    }
    /// Write-ahead intent log for the in-flight NON-IDEMPOTENT action
    /// (run_shell). Written just before the side effect, removed just
    /// after. A leftover file at beat start means the previous beat died mid
    /// side-effect — surfaced so a half-run command isn't silently re-fired.
    pub fn action_wal(&self) -> PathBuf {
        self.data_dir.join(".action-wal.json")
    }
    /// The previous beat's FAILURE record (`{ts,run_id,code,error}`), written on
    /// a failed decide and cleared on the next usable decision. Surfaced in the
    /// decide prompt (`LAST FAILURE`) so the decider can correct instead of
    /// blindly re-emitting the same failing move.
    pub fn last_failure(&self) -> PathBuf {
        self.data_dir.join(".last-failure.json")
    }
    /// The world-item baseline behind the prompt's `WHAT CHANGED` section: a
    /// JSON object mapping item name (playbook / goal:<id> / snap:<name>) to its
    /// digest or wake-signal, committed alongside `.last-tick-hash` on a usable
    /// decision. The next decide prompt diffs the live world against it.
    pub fn last_world(&self) -> PathBuf {
        self.data_dir.join(".last-world.json")
    }
    /// Durable one-shot cadence nudge: `{"due": <unix>}`. Written when a decision
    /// carries `next_interval_s`, consumed (forcing a re-decide) when due. A
    /// pulse crash during the sleep no longer loses the follow-up.
    pub fn next_wake(&self) -> PathBuf {
        self.data_dir.join(".next-wake.json")
    }
    /// The last noop decision (`{ts,hash}`). When the world hash still matches
    /// after `LOOOP_NOOP_TTL` seconds, the beat re-decides instead of skipping —
    /// a single wrong noop can no longer park a world state forever.
    pub fn noop_at(&self) -> PathBuf {
        self.data_dir.join(".noop-at.json")
    }
    /// Durable time triggers (`schedules/<name>.json`): one-shot (`at`) or
    /// recurring (`every_s`). Fed to the world hash through the `sys-schedules`
    /// system sensor, so a due schedule WAKES the loop level-triggered — no
    /// in-memory timer to lose.
    pub fn schedules_dir(&self) -> PathBuf {
        self.data_dir.join("schedules")
    }
    /// Mailbox: steering messages a human sends INTO a running worker
    /// (`looop tell <worker> …`). Drained by the worker via `looop told` and
    /// piggybacked on `looop ask` answers.
    pub fn tells_dir(&self) -> PathBuf {
        self.data_dir.join("tells")
    }
    /// Rolling history of PLAYBOOK.md: before every overwrite (`write_playbook`
    /// — the decider's OR the human's) the previous body is snapshotted to
    /// `playbook.d/<ts>.md`. The PLAYBOOK is the most valuable human-authored
    /// artifact in the loop and the write API is whole-file replacement, so one
    /// bad rewrite must never be able to destroy it unrecoverably. Pruned to
    /// `LOOOP_PLAYBOOK_KEEP` generations (default 20).
    pub fn playbook_history_dir(&self) -> PathBuf {
        self.data_dir.join("playbook.d")
    }
    /// The output tail of the LAST executed `run_shell` move
    /// (`{v,ts,cmd,exit_code,output}`). Surfaced in the next decide prompt
    /// (`RUN_SHELL OUTPUT`) so a query's result actually reaches the decider —
    /// without this, run_shell stdout went nowhere and "query" moves were
    /// structurally useless. Consumed (removed) when the next decision executes.
    pub fn last_shell(&self) -> PathBuf {
        self.data_dir.join(".last-shell.json")
    }
    /// Per-snapshot signal-change streaks (`{v,snaps:{name:{last,streak}}}`).
    /// A sensor whose wake SIGNAL changes on every consecutive beat defeats
    /// both the unchanged-world skip and the failure backoff (the hash never
    /// settles), so the whole economics of the loop hinge on catching it. The
    /// beat updates this ledger after sensing; a streak at/over the threshold
    /// is surfaced in the prompt (`FLAPPING SENSORS`) so the decider fixes the
    /// sensor instead of burning a decide per beat forever.
    pub fn flap_state(&self) -> PathBuf {
        self.data_dir.join(".signal-flap.json")
    }
    /// Rolling ledger of decide attempts (`{v,ts:[unix,…]}`), pruned to the
    /// last hour. Backs the global spend cap (`LOOOP_MAX_DECIDES_PER_HOUR`):
    /// the skip gate and backoff bound a QUIET loop's cost, but nothing else
    /// bounds a noisy one (a flapping sensor + cadence nudges can reach one
    /// decide per 5s), so this is the hard ceiling underneath both.
    pub fn decide_ledger(&self) -> PathBuf {
        self.data_dir.join(".decide-ledger.json")
    }

    /// A throwaway `Paths` rooted at a freshly-created temp data dir. Test-only.
    /// The dir is deleted when the value drops (see the `Drop` impl below), so
    /// a test run no longer strews `looop-test-*` dirs across the system temp
    /// dir. Tests hold their `Paths` by value for their whole body (moving it
    /// into a thread joins before the assertions end), so drop-at-test-end is
    /// exactly the teardown point.
    #[cfg(test)]
    pub fn temp() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("looop-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        Paths {
            bin: PathBuf::from("looop"),
            data_dir: dir.clone(),
            config: dir.join("config.json"),
            default_profile: false,
            temp_cleanup: true,
        }
    }
}

/// Test-only teardown for [`Paths::temp`]: remove the throwaway data dir when
/// the test's `Paths` drops. Gated on `temp_cleanup` so a `resolve()`d Paths
/// (real state) can never be deleted, even in a test build. Best-effort — a
/// failed cleanup must never panic across a test's own result.
#[cfg(test)]
impl Drop for Paths {
    fn drop(&mut self) {
        if self.temp_cleanup {
            let _ = std::fs::remove_dir_all(&self.data_dir);
        }
    }
}
