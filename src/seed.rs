//! First-run seeding + directory layout.
//!
//! Config is seeded inline (config.rs); data (memory) starts empty except for an
//! embedded starter PLAYBOOK + goals + heartbeat sensor, written ONCE. Setup is
//! then just a goal: the starter PLAYBOOK's top priority is an interactive setup
//! session that rewrites the seed into the user's real config. The program makes
//! no decisions — it only lays down bytes.

use crate::config;
use crate::paths::Paths;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

const SEED_PLAYBOOK: &str = include_str!("seed/PLAYBOOK.md");
const SEED_GOAL_SETUP: &str = include_str!("seed/setup.md");
const SEED_GOAL_PLAYBOOK_DAILY: &str = include_str!("seed/playbook-daily.md");
const SEED_SENSOR_TODAY: &str = include_str!("seed/today.sh");

const GITIGNORE: &str = "\
snapshots/
prompts/
runs/
claims/
reports/
.lock/
.last-tick-hash
.next-interval
.tick-backoff
.cost-unmetered
tick.log
events.jsonl
cost.jsonl
# worker scratch that can land in the data dir
.ruff_cache/
.pytest_cache/
.mypy_cache/
__pycache__/
";

/// Create the data/config layout, seed config + starter memory + .gitignore.
/// Idempotent (mirrors the bash `ensure_dirs`).
pub fn ensure_dirs(paths: &Paths) -> Result<()> {
    for d in [
        paths
            .config
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf(),
        paths.sensors_dir(),
        paths.snapshots_dir(),
        paths.claims_dir(),
        paths.reports_dir(),
        paths.runs_dir(),
        paths.goals_dir().join("archive"),
        paths.prompts_dir(),
    ] {
        fs::create_dir_all(&d).with_context(|| format!("mkdir -p {}", d.display()))?;
    }

    config::ensure_config(paths)?;

    // Fresh data dir (no PLAYBOOK yet) -> lay down the embedded starter seed.
    if !paths.playbook().is_file() {
        seed_data(paths)?;
    }

    // Seed a .gitignore so the data dir versions cleanly IF the user chooses to
    // `git init` it (looop itself does not): track policy/journal, ignore scratch.
    let gi = paths.data_dir.join(".gitignore");
    if !gi.is_file() {
        fs::write(&gi, GITIGNORE).with_context(|| format!("writing {}", gi.display()))?;
    }
    Ok(())
}

/// Write the embedded starter seed once.
fn seed_data(paths: &Paths) -> Result<()> {
    fs::write(paths.playbook(), SEED_PLAYBOOK)?;
    fs::write(paths.goals_dir().join("setup.md"), SEED_GOAL_SETUP)?;
    fs::write(
        paths.goals_dir().join("playbook-daily.md"),
        SEED_GOAL_PLAYBOOK_DAILY,
    )?;
    let sensor = paths.sensors_dir().join("today.sh");
    fs::write(&sensor, SEED_SENSOR_TODAY)?;
    make_executable(&sensor)?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(p)?.permissions();
    perm.set_mode(0o755);
    fs::set_permissions(p, perm)?;
    Ok(())
}
#[cfg(not(unix))]
fn make_executable(_p: &Path) -> Result<()> {
    Ok(())
}
