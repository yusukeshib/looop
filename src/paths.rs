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
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .expect("looop: $HOME is not set")
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
            Some(v) if !v.is_empty() => PathBuf::from(v),
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
    /// Mailbox: questions a worker raises for the root agent (`looop _ ask`).
    /// One JSON file per ask (`asks/<worker>-<n>.json`); a matching
    /// `answers/<worker>-<n>.json` resolves it. Durable + level-triggered — a
    /// crashed pulse re-reads the unanswered asks on restart.
    pub fn asks_dir(&self) -> PathBuf {
        self.data_dir.join("asks")
    }
    /// Mailbox: answers the root agent writes back (`looop _ answer`).
    pub fn answers_dir(&self) -> PathBuf {
        self.data_dir.join("answers")
    }
    pub fn cost_ledger(&self) -> PathBuf {
        self.data_dir.join("cost.jsonl")
    }
    pub fn prompts_dir(&self) -> PathBuf {
        self.data_dir.join("prompts")
    }
    /// Per-goal "last acted" ledger (goal id -> RFC3339 ts). Drives the
    /// `sys-goals` staleness reading so the decider can avoid starving a goal.
    pub fn goal_activity(&self) -> PathBuf {
        self.data_dir.join(".goal-activity.json")
    }
    /// Write-ahead intent log for the in-flight NON-IDEMPOTENT action (run_shell
    /// / send_notification). Written just before the side effect, removed just
    /// after. A leftover file at beat start means the previous beat died mid
    /// side-effect — surfaced so a half-run command isn't silently re-fired.
    pub fn action_wal(&self) -> PathBuf {
        self.data_dir.join(".action-wal.json")
    }

    /// A throwaway `Paths` rooted at a freshly-created temp data dir. Test-only.
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
        }
    }
}
