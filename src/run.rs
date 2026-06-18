//! The pulse (`looop`) and the manual one-shot (`looop run <goal>`).
//!
//! The pulse is a single-instance, level-triggered reconcile loop: tick, choose
//! the next cadence, sleep, repeat. The manual run forces ONE goal-focused move
//! immediately, bypassing the priority order and the world-unchanged skip.

use crate::config::Config;
use crate::paths::Paths;
use crate::{babysit, gate, prompt, runner, sensor, seed, surface, tick, util};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Resolve a cadence knob: env var > config key > fallback.
fn interval(env: &str, cfg: &Config, key: &str, fallback: u64) -> u64 {
    if let Ok(v) = std::env::var(env) {
        if let Ok(n) = v.trim().parse::<u64>() {
            return n;
        }
    }
    cfg.root
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .unwrap_or(fallback)
}

/// Removes the lock dir on drop (covers normal exit / panics; a hard Ctrl-C
/// relies on the stale-lock reclaim in the next start, which checks pid liveness).
struct LockGuard {
    path: PathBuf,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn cmd_run(paths: &Paths) -> Result<ExitCode> {
    seed::ensure_dirs(paths)?;
    let cfg = Config::load(paths)?;
    let idle = interval("LOOOP_INTERVAL", &cfg, "interval", 60);
    let busy = interval("LOOOP_BUSY_INTERVAL", &cfg, "busy_interval", idle);
    let active = interval("LOOOP_ACTIVE_INTERVAL", &cfg, "active_interval", idle);

    // Single-instance lock (mkdir-based; macOS has no flock).
    let lock = paths.lock();
    if fs::create_dir(&lock).is_err() {
        let oldpid = fs::read_to_string(lock.join("pid")).unwrap_or_default();
        let oldpid = oldpid.trim();
        if util::pid_alive(oldpid) {
            eprintln!("looop: already running (pid {oldpid})");
            return Ok(ExitCode::from(1));
        }
        // Stale lock: reclaim it (mkdir is the atomic arbiter on the race).
        let _ = fs::remove_dir_all(&lock);
        if fs::create_dir(&lock).is_err() {
            eprintln!("looop: lost the lock race — another pulse started; exiting");
            return Ok(ExitCode::from(1));
        }
    }
    let _ = fs::write(lock.join("pid"), format!("{}\n", std::process::id()));
    let _guard = LockGuard { path: lock.clone() };

    let runner_name = cfg.default_runner().unwrap_or_else(|| "?".into());
    util::log(&format!(
        "{}looop started{} {}(idle {}s / busy {}s, runner {}){}",
        util::b(),
        util::rst(),
        util::dim(),
        idle,
        busy,
        runner_name,
        util::rst()
    ));
    if let Some(bd) = &paths.babysit_dir {
        util::log(&format!(
            "{}profile fleet: BABYSIT_DIR={d} (attach with: BABYSIT_DIR={d} babysit ls){}",
            util::dim(),
            util::rst(),
            d = bd.display()
        ));
    }

    loop {
        let acted = tick::tick(paths);
        let mut want = if acted {
            busy
        } else if babysit::any_looop_alive() {
            active
        } else {
            idle
        };

        // One-shot AI cadence override via .next-interval (clamped 5..3600).
        let reqf = paths.data_dir.join(".next-interval");
        if let Ok(raw) = fs::read_to_string(&reqf) {
            let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
            let _ = fs::remove_file(&reqf);
            if let Ok(mut req) = digits.parse::<u64>() {
                req = req.clamp(5, 3600);
                util::log(&format!(
                    "{}next beat in {req}s (AI cadence; default {want}s){}",
                    util::dim(),
                    util::rst()
                ));
                want = req;
            }
        }
        std::thread::sleep(Duration::from_secs(want));
    }
}

/// Cleans up a private snapshots temp dir on drop.
struct TempDir {
    path: PathBuf,
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn cmd_run_goal(paths: &Paths, id: &str) -> Result<ExitCode> {
    seed::ensure_dirs(paths)?;

    let mut gf = paths.goals_dir().join(format!("{id}.md"));
    if !gf.is_file() {
        gf = paths.goals_dir().join("archive").join(format!("{id}.md"));
    }
    if !gf.is_file() {
        eprintln!("looop run: no such goal '{id}' (looked in goals/ and goals/archive/)");
        eprintln!("available goals:");
        let mut names: Vec<String> = fs::read_dir(paths.goals_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "md").unwrap_or(false))
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .collect();
        names.sort();
        for n in names {
            eprintln!("  {n}");
        }
        return Ok(ExitCode::from(1));
    }

    // Manual override: note (don't refuse) if the pulse is running.
    let lock = paths.lock();
    if lock.is_dir() {
        let pid = fs::read_to_string(lock.join("pid")).unwrap_or_default();
        let pid = pid.trim();
        if util::pid_alive(pid) {
            util::log(&format!(
                "{}note:{} pulse is running (pid {pid}) — running goal '{id}' anyway {}(manual override){}",
                util::yel(),
                util::rst(),
                util::dim(),
                util::rst()
            ));
        }
    }

    babysit::prune();
    gate::reap_stale_claims(paths);
    gate::playbook_gate_pre(paths);

    // Private snapshots dir so a concurrent pulse tick can't tear our readings.
    let snap = std::env::temp_dir().join(format!(
        "looop-run.{}-{}",
        std::process::id(),
        chrono::Local::now().format("%H%M%S%f")
    ));
    let _ = fs::create_dir_all(&snap);
    let _tmp = TempDir { path: snap.clone() };
    sensor::run_all(paths, &snap, false);

    let cfg = Config::load(paths)?;
    let runner_name = cfg.default_runner().unwrap_or_default();
    let Some(tick_cmd) = cfg.runner_cmd(&runner_name, "tick") else {
        eprintln!("looop run: no tick command for runner '{runner_name}'");
        return Ok(ExitCode::from(1));
    };

    let run_id = format!(
        "run-{id}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let run_dir = paths.runs_dir().join(&run_id);
    let _ = fs::create_dir_all(&run_dir);
    let prompt_file = run_dir.join("prompt.md");
    let _ = fs::write(&prompt_file, prompt::build_prompt(paths, Some(id), &snap));

    let t0 = Instant::now();
    util::log(&format!(
        "{}{}manual run — goal '{id}'{} {}({} is thinking; its output follows){}…",
        util::cyan(),
        util::b(),
        util::rst(),
        util::dim(),
        runner_name,
        util::rst()
    ));
    util::log(&format!(
        "{}┌─ {} ──────────────────────────────────────{}",
        util::dim(),
        runner_name,
        util::rst()
    ));

    let tee: Vec<PathBuf> = vec![run_dir.join("output.log"), paths.data_dir.join("tick.log")];
    let cost_env = [
        ("LOOOP_COST_KIND", "goal"),
        ("LOOOP_COST_RUNNER", runner_name.as_str()),
        ("LOOOP_COST_ID", id),
    ];
    runner::run_streamed(paths, &tick_cmd, &prompt_file, &cost_env, &tee, "");

    util::log(&format!(
        "{}└────────────────────────────────────────{}",
        util::dim(),
        util::rst()
    ));
    let last_line = fs::read_to_string(paths.journal())
        .ok()
        .and_then(|j| j.lines().last().map(str::to_owned))
        .unwrap_or_else(|| "(no line)".into());
    util::log(&format!(
        "{}✓ run done{} in {}s {}— journal: {} · replay: {}{}",
        util::grn(),
        util::rst(),
        t0.elapsed().as_secs(),
        util::dim(),
        last_line,
        run_dir.display(),
        util::rst()
    ));

    gate::playbook_gate_post(paths);
    tick::prune_runs(paths);
    surface::surface_attention(paths);
    Ok(ExitCode::SUCCESS)
}
