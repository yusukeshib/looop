//! First-run seeding + directory layout.
//!
//! Config (the runner wiring) is written separately by `looop init`; this module
//! only lays down the DATA dir: an embedded starter PLAYBOOK + goals + heartbeat
//! sensor, written ONCE. Setup is surfaced as a real pending Ask so a first-run
//! concierge waiting on `looop wait --only-asks` wakes immediately and can start
//! the interview that rewrites the seed into the user's real config. The program
//! makes no decisions — it only lays down bytes.

use crate::mailbox::Ask;
use crate::paths::Paths;
use crate::store::{FileStore, Key, StateStore};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

const SEED_PLAYBOOK: &str = include_str!("seed/PLAYBOOK.md");
const SEED_GOAL_SETUP: &str = include_str!("seed/setup.md");
const SEED_GOAL_PLAYBOOK_DAILY: &str = include_str!("seed/playbook-daily.md");
const SEED_SENSOR_TODAY: &str = include_str!("seed/today.sh");

const SEED_SETUP_ASK_ID: &str = "setup-1";
const SEED_SETUP_ASK_PROMPT: &str = "First-run setup: looop is unconfigured. Please have your concierge interview you about goals, sensors, irreversible actions, repos/workspaces, and recurring cadences, then write the real PLAYBOOK/goals/sensors and archive the setup goal.";

// Runtime scratch only — durable POLICY stays tracked on purpose: schedules/
// (durable time triggers) and playbook.d/ (PLAYBOOK history) are part of the
// steering record a user would want in git, so they are NOT listed here.
const GITIGNORE: &str = "\
snapshots/
prompts/
runs/
claims/
reports/
asks/
answers/
tells/
sessions/
verify/
.lock/
.last-tick-hash
.tick-backoff
.goal-activity.json
.last-world.json
.last-shell.json
.last-failure.json
.signal-flap.json
.decide-ledger.json
.noop-at.json
.next-wake.json
tick.log
events.jsonl
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
        paths.asks_dir(),
        paths.answers_dir(),
        paths.runs_dir(),
        paths.goals_dir().join("archive"),
        paths.prompts_dir(),
        paths.schedules_dir(),
        paths.tells_dir(),
    ] {
        fs::create_dir_all(&d).with_context(|| format!("mkdir -p {}", d.display()))?;
    }

    // Config is NOT written here: `looop init` writes the runner wiring, and an
    // absent config is the "not initialized" signal. Until then the loop runs on
    // the inline DEFAULT_CONFIG (config::Config::load falls back to it).

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
    let store = FileStore::new(paths);
    store.write_atomic(&Key::Playbook, SEED_PLAYBOOK)?;
    store.write_atomic(&Key::Goal("setup".into()), SEED_GOAL_SETUP)?;
    store.write_atomic(
        &Key::Goal("playbook-daily".into()),
        SEED_GOAL_PLAYBOOK_DAILY,
    )?;
    // write_atomic sets the sensor's exec bit (it's a script the runtime runs).
    store.write_atomic(&Key::Sensor("today".into()), SEED_SENSOR_TODAY)?;
    seed_setup_ask(&store)?;
    Ok(())
}

fn seed_setup_ask(store: &impl StateStore) -> Result<()> {
    // A fresh loop must be visible to the thinnest concierge, which normally
    // blocks on `looop wait --only-asks`. Journal-only setup notices are too
    // easy to miss, so seed a real mailbox item. It is intentionally from the
    // synthetic `setup` worker: no worker is blocked on the answer; the concierge
    // uses the pending ask as the human-facing trigger for the setup interview.
    let ask = Ask {
        id: SEED_SETUP_ASK_ID.to_string(),
        worker: "setup".to_string(),
        prompt: SEED_SETUP_ASK_PROMPT.to_string(),
        reference: "goals/setup.md".to_string(),
        options: vec![],
        legacy_detached: false,
        ts: crate::util::now_unix(),
    };
    store.write_atomic(
        &Key::Ask(SEED_SETUP_ASK_ID.to_string()),
        &serde_json::to_string_pretty(&ask)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_seed_contains_pending_setup_ask() {
        let p = Paths::temp();
        ensure_dirs(&p).unwrap();

        let asks = crate::mailbox::pending(&p);
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].id, SEED_SETUP_ASK_ID);
        assert_eq!(asks[0].worker, "setup");
        assert_eq!(asks[0].reference, "goals/setup.md");
    }
}
