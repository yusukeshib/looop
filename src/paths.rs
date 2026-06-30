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
use std::path::{Path, PathBuf};

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

/// If invoked from inside a workspace that already has a real `.looop` data dir,
/// prefer that over the process-global XDG default. This keeps client reads like
/// `looop _ asks --json` from silently inspecting `~/.local/state/looop` when the
/// caller is standing in a repo-local loop.
fn workspace_data_dir() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    for dir in cwd.ancestors() {
        if is_looop_data_dir(dir) {
            return Some(dir.to_path_buf());
        }

        let candidate = dir.join(".looop");
        if is_looop_data_dir(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_looop_data_dir(path: &Path) -> bool {
    path.is_dir()
        && path.join("config.json").is_file()
        && path.join("PLAYBOOK.md").is_file()
        && path.join("goals").is_dir()
}

impl Paths {
    pub fn resolve() -> Self {
        let bin = env::current_exe().unwrap_or_else(|_| PathBuf::from("looop"));

        let default_data = xdg("XDG_STATE_HOME", ".local/state").join("looop");
        let data_dir = match env::var_os("LOOOP_DATA_DIR") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => workspace_data_dir().unwrap_or_else(|| default_data.clone()),
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
    /// Mailbox: questions a worker raises for the human (`looop _ ask`).
    /// One JSON file per ask (`asks/<worker>-<n>.json`); a matching
    /// `answers/<worker>-<n>.json` resolves it. Durable + level-triggered — a
    /// crashed pulse re-reads the unanswered asks on restart.
    pub fn asks_dir(&self) -> PathBuf {
        self.data_dir.join("asks")
    }
    /// Mailbox: answers the human writes back (`looop _ answer`).
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    };

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static N: AtomicU64 = AtomicU64::new(0);

    struct RestoreEnv {
        cwd: PathBuf,
        home: Option<OsString>,
        xdg_state_home: Option<OsString>,
        data_dir: Option<OsString>,
        config: Option<OsString>,
    }

    impl RestoreEnv {
        fn capture() -> Self {
            Self {
                cwd: env::current_dir().expect("current dir"),
                home: env::var_os("HOME"),
                xdg_state_home: env::var_os("XDG_STATE_HOME"),
                data_dir: env::var_os("LOOOP_DATA_DIR"),
                config: env::var_os("LOOOP_CONFIG"),
            }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.cwd);
            restore_var("HOME", self.home.as_ref());
            restore_var("XDG_STATE_HOME", self.xdg_state_home.as_ref());
            restore_var("LOOOP_DATA_DIR", self.data_dir.as_ref());
            restore_var("LOOOP_CONFIG", self.config.as_ref());
        }
    }

    fn restore_var(key: &str, value: Option<&OsString>) {
        unsafe {
            match value {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
        }
    }

    fn set_var(key: &str, value: impl AsRef<std::ffi::OsStr>) {
        unsafe { env::set_var(key, value) }
    }

    fn unset_var(key: &str) {
        unsafe { env::remove_var(key) }
    }

    fn temp_root(label: &str) -> PathBuf {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("looop-paths-{label}-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn write_data_dir(dir: &Path) {
        fs::create_dir_all(dir.join("goals")).expect("create goals");
        fs::write(dir.join("config.json"), "{}\n").expect("write config");
        fs::write(dir.join("PLAYBOOK.md"), "# playbook\n").expect("write playbook");
    }

    fn prepare_env(root: &Path) {
        set_var("HOME", root.join("home"));
        unset_var("XDG_STATE_HOME");
        unset_var("LOOOP_DATA_DIR");
        unset_var("LOOOP_CONFIG");
    }

    #[test]
    fn uses_workspace_data_dir_when_env_is_unset() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = RestoreEnv::capture();
        let root = temp_root("workspace");
        prepare_env(&root);
        let raw_data = root.join(".looop");
        write_data_dir(&raw_data);
        let data = fs::canonicalize(&raw_data).expect("canonicalize data dir");

        env::set_current_dir(&root).expect("chdir workspace");
        let paths = Paths::resolve();

        assert_eq!(paths.data_dir, data);
        assert_eq!(paths.config, paths.data_dir.join("config.json"));
        assert!(!paths.default_profile);
    }

    #[test]
    fn finds_workspace_data_dir_from_subdirectories() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = RestoreEnv::capture();
        let root = temp_root("subdir");
        prepare_env(&root);
        let raw_data = root.join(".looop");
        write_data_dir(&raw_data);
        let data = fs::canonicalize(&raw_data).expect("canonicalize data dir");
        let nested = root.join("notes/day");
        fs::create_dir_all(&nested).expect("create nested dir");

        env::set_current_dir(&nested).expect("chdir nested");
        let paths = Paths::resolve();

        assert_eq!(paths.data_dir, data);
    }

    #[test]
    fn explicit_data_dir_wins_over_workspace_data_dir() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = RestoreEnv::capture();
        let root = temp_root("explicit");
        prepare_env(&root);
        write_data_dir(&root.join(".looop"));
        let explicit = root.join("explicit-data");
        set_var("LOOOP_DATA_DIR", &explicit);

        env::set_current_dir(&root).expect("chdir workspace");
        let paths = Paths::resolve();

        assert_eq!(paths.data_dir, explicit);
        assert_eq!(paths.config, paths.data_dir.join("config.json"));
    }

    #[test]
    fn falls_back_to_xdg_default_when_no_workspace_data_dir_exists() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = RestoreEnv::capture();
        let root = temp_root("default");
        let xdg = root.join("xdg-state");
        prepare_env(&root);
        set_var("XDG_STATE_HOME", &xdg);

        env::set_current_dir(&root).expect("chdir root");
        let paths = Paths::resolve();

        assert_eq!(paths.data_dir, xdg.join("looop"));
        assert!(paths.default_profile);
    }
}
