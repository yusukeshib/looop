//! ONE BEAT — sense → diff → decide ONE move → act → log. The heart of the
//! control loop (RULE 1: one tick = one move). Stateless and disposable: all
//! memory is the files in the data dir.

use crate::config::Config;
use crate::paths::Paths;
use crate::util::Level;
use crate::{events, executor, gate, prompt, runner, seed, sensor, session, util};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

/// Run one beat. Returns whether the AI actually acted (drives cadence).
pub fn tick(paths: &Paths) -> bool {
    let _ = seed::ensure_dirs(paths);
    events::emit(paths, "tick_start", serde_json::json!({}));

    // 0. housekeeping (deterministic, no AI). Reap only AGED corpses — a worker
    // that just finished keeps its transcript for the retention window; sessions/
    // is looop-owned scratch, bounded here, never the user's deliverables.
    session::prune_aged(
        paths,
        std::time::Duration::from_secs(crate::run::session_ttl_secs(paths)),
    );
    gate::reap_stale_claims(paths);

    // 1. sense — level-triggered: wipe last beat's snapshots first.
    let snap = paths.snapshots_dir();
    let _ = fs::remove_dir_all(&snap);
    let _ = fs::create_dir_all(&snap);
    sensor::run_all(paths, &snap, true);
    events::emit(paths, "sense_done", serde_json::json!({}));

    // 2. skip if the world is unchanged (no AI call).
    let hash = crate::worldhash::world_hash(paths);
    let last = fs::read_to_string(paths.data_dir.join(".last-tick-hash"))
        .unwrap_or_default()
        .trim()
        .to_string();
    if hash == last {
        util::event(
            Level::Info,
            "tick.skip",
            "world unchanged — no AI call",
            &[],
        );
        events::emit(paths, "world_unchanged", serde_json::json!({}));
        return false;
    }

    // 3. hand everything to the AI for one move.
    let cfg = match Config::load(paths) {
        Ok(c) => c,
        Err(e) => {
            util::event(Level::Error, "tick.error", &format!("config: {e}"), &[]);
            return false;
        }
    };
    let runner_name = cfg.default_runner().unwrap_or_default();
    let Some(tick_cmd) = cfg.runner_cmd(&runner_name, "tick") else {
        util::event(
            Level::Error,
            "tick.error",
            &format!("no tick command for runner '{runner_name}'"),
            &[("runner", serde_json::json!(runner_name))],
        );
        return false;
    };

    let cost_id = format!("tick-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let run_dir = paths.runs_dir().join(&cost_id);
    let _ = fs::create_dir_all(&run_dir);
    let prompt_file = run_dir.join("prompt.md");
    let _ = fs::write(&prompt_file, prompt::build_prompt(paths, &snap));

    let t0 = Instant::now();
    util::event(
        Level::Step,
        "tick.start",
        &format!("{runner_name} is deciding the one move"),
        &[
            ("runner", serde_json::json!(runner_name)),
            ("run_id", serde_json::json!(cost_id)),
        ],
    );
    events::emit(
        paths,
        "decide_start",
        serde_json::json!({ "runner": runner_name, "run_id": cost_id }),
    );

    // The pulse stream stays a clean structured-event log: the runner's
    // free-form chatter (its `→ bash:` calls, blank lines, final text) is
    // archived to the tee files but NOT echoed live (live=false), so
    // `looop watch pulse` shows only `tick.*`/`sense.*` events. Replay the full
    // detail from runs/<id>/output.log or tick.log.
    let tee: Vec<PathBuf> = vec![run_dir.join("output.log"), paths.data_dir.join("tick.log")];
    let cost_env = [
        ("LOOOP_COST_KIND", "tick"),
        ("LOOOP_COST_RUNNER", runner_name.as_str()),
        ("LOOOP_COST_ID", cost_id.as_str()),
    ];

    let mut acted = false;
    if runner::run_streamed(paths, &tick_cmd, &prompt_file, &cost_env, &tee) {
        let _ = fs::write(paths.data_dir.join(".last-tick-hash"), format!("{hash}\n"));
        acted = true;
        let secs = t0.elapsed().as_secs();
        let cost = tick_cost(paths, &cost_id);
        let cost_str = cost.map(|c| format!(" · ${c:.4}")).unwrap_or_default();

        // Typed-action path: looop is the SOLE executor of the decider's single
        // move. The runner emits ONE JSON action to .decision.json; we execute
        // it, journal it, and render one typed line (action · why · time · $).
        match executor::consume_decision(paths) {
            Some(Ok(d)) => {
                util::event(
                    Level::Ok,
                    "tick.decided",
                    &format!("{} · {} · {secs}s{cost_str}", d.kind, d.journal),
                    &[
                        ("action", serde_json::json!(d.kind)),
                        ("summary", serde_json::json!(d.summary)),
                        ("journal", serde_json::json!(d.journal)),
                        ("secs", serde_json::json!(secs)),
                        ("cost_usd", serde_json::json!(cost)),
                        ("run_id", serde_json::json!(cost_id)),
                    ],
                );
                events::emit(
                    paths,
                    "decided",
                    serde_json::json!({ "run_id": cost_id, "action": d.kind, "journal": d.journal }),
                );
            }
            Some(Err(e)) => {
                util::event(
                    Level::Error,
                    "tick.failed",
                    &format!(
                        "decision failed after {secs}s: {e} · replay: {}",
                        run_dir.display()
                    ),
                    &[
                        ("secs", serde_json::json!(secs)),
                        ("run_id", serde_json::json!(cost_id)),
                        ("error", serde_json::json!(e.to_string())),
                    ],
                );
                events::emit(
                    paths,
                    "tick_failed",
                    serde_json::json!({ "run_id": cost_id }),
                );
            }
            None => util::event(
                Level::Warn,
                "tick.no_decision",
                &format!(
                    "ran {secs}s but emitted no .decision.json (no move) · replay: {}",
                    run_dir.display()
                ),
                &[
                    ("secs", serde_json::json!(secs)),
                    ("run_id", serde_json::json!(cost_id)),
                ],
            ),
        }
    } else {
        let secs = t0.elapsed().as_secs();
        util::event(
            Level::Error,
            "tick.failed",
            &format!("tick failed after {secs}s · replay: {}", run_dir.display()),
            &[
                ("secs", serde_json::json!(secs)),
                ("run_id", serde_json::json!(cost_id)),
                ("replay", serde_json::json!(run_dir.display().to_string())),
            ],
        );
        events::emit(
            paths,
            "tick_failed",
            serde_json::json!({ "run_id": cost_id }),
        );
    }

    prune_runs(paths);
    acted
}

/// Best-effort: this tick's recorded spend, read back from the cost ledger (the
/// runner's `_fmt` seam writes the row before `run_streamed` returns). `None`
/// when the runner doesn't meter (e.g. the claude tick) or nothing was recorded.
fn tick_cost(paths: &Paths, cost_id: &str) -> Option<f64> {
    let text = fs::read_to_string(paths.cost_ledger()).ok()?;
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|r| r.get("id").and_then(|x| x.as_str()) == Some(cost_id))
        .and_then(|r| r.get("cost_usd").and_then(|c| c.as_f64()))
}

/// Keep the newest LOOOP_RUNS_KEEP run dirs (default 50; 0 = keep all).
pub fn prune_runs(paths: &Paths) {
    let keep: usize = std::env::var("LOOOP_RUNS_KEEP")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(50);
    if keep == 0 {
        return;
    }
    let dir = paths.runs_dir();
    let mut runs: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let m = e.metadata().ok()?.modified().ok()?;
            Some((m, e.path()))
        })
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.0)); // newest first
    for (_, p) in runs.into_iter().skip(keep) {
        let _ = fs::remove_dir_all(p);
    }
}

/// `looop tick` — one beat, refusing while the pulse holds a live lock.
pub fn cmd_tick(paths: &Paths) -> Result<ExitCode> {
    let lock = paths.lock();
    if lock.is_dir() {
        let pid = fs::read_to_string(lock.join("pid")).unwrap_or_default();
        let pid = pid.trim();
        if util::pid_alive(pid) {
            eprintln!("looop: pulse already running (pid {pid}) — it ticks on its own");
            return Ok(ExitCode::from(1));
        }
    }
    tick(paths);
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_cost_reads_matching_ledger_row() {
        let p = Paths::temp();
        fs::write(
            p.cost_ledger(),
            concat!(
                r#"{"ts":"t","kind":"tick","id":"tick-A","runner":"pi","cost_usd":0.0123}"#,
                "\n",
                r#"{"ts":"t","kind":"tick","id":"tick-B","runner":"pi","cost_usd":0.0456}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(tick_cost(&p, "tick-B"), Some(0.0456));
        assert_eq!(tick_cost(&p, "tick-A"), Some(0.0123));
        assert_eq!(tick_cost(&p, "tick-missing"), None);
    }

    #[test]
    fn tick_cost_none_without_ledger() {
        let p = Paths::temp();
        assert_eq!(tick_cost(&p, "tick-X"), None);
    }
}
