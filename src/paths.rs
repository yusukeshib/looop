//! Path + profile layer — a faithful port of the bash header's path block.
//!
//! CODE / CONFIG / DATA are cleanly separated and all overridable by env:
//!   CONFIG  = $LOOOP_CONFIG          or ${XDG_CONFIG_HOME:-~/.config}/looop.json
//!   DATA    = $LOOOP_DATA_DIR        or ${XDG_STATE_HOME:-~/.local/state}/looop
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
    /// The git-tracked memory dir ($LOOOP_DATA_DIR).
    pub data_dir: PathBuf,
    /// The single runner-wiring config file ($LOOOP_CONFIG).
    pub config: PathBuf,
    /// Per-profile babysit state root, when this is a non-default profile.
    /// `None` on the default profile (leaves ~/.babysit untouched).
    pub babysit_dir: Option<PathBuf>,
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

        let config = match env::var_os("LOOOP_CONFIG") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => xdg("XDG_CONFIG_HOME", ".config").join("looop.json"),
        };

        // Worker-fleet isolation: a non-default profile (= a distinct
        // LOOOP_DATA_DIR) gets its own babysit root, derived PURELY from
        // LOOOP_DATA_DIR (it intentionally ignores any inherited BABYSIT_DIR),
        // so `babysit ls/prune/kill` are naturally scoped to THIS profile. The
        // default profile keeps ~/.babysit untouched (zero migration).
        let babysit_dir = if data_dir != default_data {
            Some(data_dir.join("babysit"))
        } else {
            None
        };

        Paths {
            bin,
            data_dir,
            config,
            babysit_dir,
        }
    }

    // ---- derived data-dir paths (mirror the bash globals) -------------------
    pub fn sensors_dir(&self) -> PathBuf {
        self.data_dir.join("sensors")
    }
    pub fn playbook(&self) -> PathBuf {
        self.data_dir.join("PLAYBOOK.md")
    }
    pub fn playbook_approved(&self) -> PathBuf {
        self.data_dir.join(".PLAYBOOK.approved")
    }
    pub fn playbook_proposed(&self) -> PathBuf {
        self.data_dir.join("PLAYBOOK.proposed.md")
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
    pub fn cost_ledger(&self) -> PathBuf {
        self.data_dir.join("cost.jsonl")
    }
    pub fn prompts_dir(&self) -> PathBuf {
        self.data_dir.join("prompts")
    }

    /// `looop ...` hint prefix that survives a fresh shell (e.g. a tmux window).
    /// On a non-default profile the fleet lives under a non-default
    /// LOOOP_DATA_DIR, which a bare `looop` invocation wouldn't know about, so
    /// emit `LOOOP_DATA_DIR=... ` to re-scope it. Empty on the default profile.
    pub fn looop_hint_env(&self) -> String {
        match &self.babysit_dir {
            Some(_) => format!("LOOOP_DATA_DIR={} ", self.data_dir.display()),
            None => String::new(),
        }
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
            config: dir.join("looop.json"),
            babysit_dir: None,
        }
    }
}
