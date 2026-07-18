//! SENSE — run every `sensors/*.sh`, each printing one JSON snapshot of the
//! world. Two guardrails keep a misbehaving sensor from harming the pulse:
//!   * an in-process timeout (LOOOP_SENSOR_TIMEOUT, default 60s — no external
//!     `timeout`/`gtimeout` binary needed) so a hung sensor can't freeze the beat;
//!   * a size cap (LOOOP_SENSOR_MAX_BYTES, default 8192) so an oversized blob
//!     can't silently inflate prompt context + LLM cost on every beat.

use crate::paths::Paths;
use crate::session;
use crate::store::StateStore;
use crate::util::{self, Level};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

fn env_num(var: &str, default: u64) -> u64 {
    crate::util::env_knob(var).unwrap_or(default)
}

/// Read up to the last `max` bytes of a file as a trimmed lossy string (the
/// stderr tail surfaced to the decider on a sensor failure). Empty when absent.
fn read_tail(path: &Path, max: usize) -> String {
    let bytes = fs::read(path).unwrap_or_default();
    let start = bytes.len().saturating_sub(max);
    String::from_utf8_lossy(&bytes[start..]).trim().to_string()
}

/// Run ONE sensor with an IN-PROCESS timeout + size cap. Returns its exit
/// status (124 = timed out, matching the coreutils convention; 128+signo for
/// a signal death). `out`/`err` receive stdout/stderr — stdout streams into a
/// sibling temp file (no pipe to drain) and is rename-published to `out` only
/// on success, so watchers never observe a partial snapshot.
///
/// The timeout deliberately depends on NO external `timeout`/`gtimeout`
/// binary (plain macOS ships neither): the child runs in its own process
/// group (`process_group(0)`), `try_wait` is polled against the
/// LOOOP_SENSOR_TIMEOUT budget, and on expiry the WHOLE group is killed
/// (negative-pid kill) so even helpers the script forked die with it — the
/// "a hung sensor can't freeze the beat" guarantee holds on every platform.
fn exec_sensor(script: &Path, out: &Path, err: &Path) -> i32 {
    let to = env_num("LOOOP_SENSOR_TIMEOUT", 60);

    // The child's stdout streams into a TEMP file (`<name>.json.tmp`, same
    // dir so the final rename is same-filesystem atomic) and is PUBLISHED to
    // the snapshot path only once the sensor succeeds. Streaming straight
    // into the snapshot violated observe.rs probe_stamp's invariant that all
    // watched writes are rename-published — a concurrent reader (state /
    // wait / the prompt) could see partial JSON and wake spuriously. The tmp
    // has a non-`json` extension, so the prompt's `sorted_glob(_, "json")`
    // never picks it up, and run_all's ownership prune clears any tmp a
    // crashed run left behind.
    let tmp = {
        let mut name = out
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(".tmp");
        out.with_file_name(name)
    };

    // Any failure (capture-file create, spawn, non-zero exit) must land as the
    // same normalized {signal,detail} error blob — rename-published via
    // write_atomic, never a torn plain write — so the decider sees the failure
    // instead of an unexplained blank/partial world. The dead tmp is dropped.
    let fail_blob = |rc: i32, msg: String| -> i32 {
        let blob = serde_json::json!({
            "signal": { "error": true, "exit_code": rc },
            "detail": { "message": msg, "stderr": read_tail(err, 1024) },
        });
        let _ = util::write_atomic(out, format!("{blob}\n").as_bytes());
        let _ = fs::remove_file(&tmp);
        rc
    };

    let (of, ef) = match (File::create(&tmp), File::create(err)) {
        (Ok(of), Ok(ef)) => (of, ef),
        // Best-effort: if the capture file itself is uncreatable the blob
        // write below likely fails too, but a failing .err file alone must
        // not silently blank the snapshot.
        _ => return fail_blob(1, "cannot create sensor capture files".to_string()),
    };

    let mut cmd = Command::new(script);
    cmd.stdout(of).stderr(ef);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group (pgid == child pid) so the timeout can kill the
        // sensor AND anything it spawned in one shot.
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return fail_blob(
                1,
                format!("spawn failed: {e} — check the script's exec bit and shebang"),
            );
        }
    };
    // checked_add: an absurd LOOOP_SENSOR_TIMEOUT (u64::MAX) would overflow
    // Instant + Duration and panic — overflow means "no deadline". `to == 0`
    // also disables the timeout.
    let deadline = if to == 0 {
        None
    } else {
        Instant::now().checked_add(Duration::from_secs(to))
    };
    // Set ONLY when the child actually died to a signal (never inferred from
    // the numeric rc): a script that itself `exit 130`s must not be reported
    // as "killed by signal 2".
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut killed_by_signal: Option<i32> = None;
    let rc = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A signal death has no exit code — report the conventional
                // 128+signo instead of a generic 1, so the error blob can say
                // "killed by signal N" (OOM kill, external TERM) rather than
                // masquerade as an ordinary script failure.
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(sig) = status.signal() {
                        killed_by_signal = Some(sig);
                        break 128 + sig;
                    }
                }
                break status.code().unwrap_or(1);
            }
            Ok(None) => {
                if let Some(d) = deadline
                    && Instant::now() >= d
                {
                    util::kill_process_group(child.id());
                    // Portable fallback: if the group kill ever fails or is a
                    // no-op (non-unix), killing the direct child keeps wait()
                    // from blocking forever (redundant ESRCH on the happy
                    // path is harmless).
                    let _ = child.kill();
                    let _ = child.wait();
                    break 124;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                // Same blast radius as the timeout path: the sensor owns its
                // process GROUP, so kill the group, not just the direct child
                // (helpers it forked must die with it).
                util::kill_process_group(child.id());
                let _ = child.kill(); // same portable fallback as the timeout path
                let _ = child.wait();
                break 1;
            }
        }
    };

    // A FAILED sensor (non-zero exit, incl. rc 124 = timed out) otherwise leaves
    // whatever partial/empty stdout it wrote as the snapshot, and its stderr only
    // lands in the .err file the prompt never reads — so the decider reasons over
    // a blank/garbage world and may noop blindly. Replace the snapshot with a
    // normalized {signal,detail} error object: the failure now reaches the prompt
    // AND the world hash (only .signal feeds it, so a stable break wakes the loop
    // once, volatile stderr in .detail doesn't re-wake it, and FIXING the sensor
    // — stdout differs again — wakes the loop next beat).
    if rc != 0 {
        let msg = if rc == 124 {
            format!("sensor timed out after {to}s (LOOOP_SENSOR_TIMEOUT)")
        } else if let Some(sig) = killed_by_signal {
            format!(
                "sensor killed by signal {sig} — an external kill (OOM, TERM), not a script bug"
            )
        } else {
            "sensor exited non-zero — fix the script or its environment".to_string()
        };
        return fail_blob(rc, msg);
    }

    // Context backpressure: a successful reading over the cap is replaced with a
    // tiny error object so the pulse stops paying for the blob and the AI sees
    // the misbehavior. Normalized {signal,detail}: the SIZE-VOLATILE byte count
    // rides in .detail — with it in the (implicit) signal, every fluctuation of
    // an oversized blob's size moved the world hash and re-woke the loop each
    // beat (self-inflicted flapping that defeats the unchanged-world skip).
    let cap = env_num("LOOOP_SENSOR_MAX_BYTES", 8192);
    if cap != 0
        && let Ok(meta) = fs::metadata(&tmp)
    {
        let sz = meta.len();
        if sz > cap {
            let blob = serde_json::json!({
                "signal": { "error": "too-large", "cap": cap },
                "detail": {
                    "bytes": sz,
                    "message": "sensor output too large — emit a small normalized {signal,detail} snapshot, not a raw dump",
                },
            });
            let _ = util::write_atomic(out, format!("{blob}\n").as_bytes());
            let _ = fs::remove_file(&tmp);
            return rc;
        }
    }
    // Publish the successful reading: rename(2) is atomic on the same
    // filesystem, so a concurrent reader sees the old snapshot or the new one
    // — never a torn write (the invariant probe_stamp documents and relies on).
    if fs::rename(&tmp, out).is_err() {
        return fail_blob(
            1,
            "cannot publish the sensor snapshot (rename failed)".to_string(),
        );
    }
    rc
}

/// A virtual system sensor: an in-process probe of looop's OWN state that
/// returns one `{signal,detail}` snapshot value.
type Probe = fn(&Paths) -> serde_json::Value;

/// One observation source. The loop senses the world through a uniform set of
/// these; the User/System split is the ONLY place the distinction lives, and
/// everything downstream treats every sensor identically.
///
/// - `User` is a `sensors/*.sh` script: authored by the decider/human, shelled
///   out with a timeout + size cap, and MAY fail.
/// - `System` is a VIRTUAL in-process [`Probe`] of looop's OWN state (the fleet,
///   the leases): no source file, no shell, no timeout, never fails.
///
/// Both write ONE `{signal,detail}` JSON snapshot into snap_dir under a
/// kind-prefixed name (`sensor-…` / `sys-…`), so the world hash and the tick
/// prompt consume one uniform snapshot stream instead of bespoke per-kind code.
enum Sensor {
    User(PathBuf),
    System { name: &'static str, probe: Probe },
}

/// The fixed set of system sensors. Expose another slice of looop's internal
/// state to the decider by adding one row + a [`Probe`].
const SYSTEM_SENSORS: &[(&str, Probe)] = &[
    ("sessions", sys_sessions),
    ("claims", sys_claims),
    ("goals", sys_goals),
    ("schedules", crate::schedule::sys_schedules),
    // The mailbox as a first-class world item: an ask raised / answered /
    // resumed changes this signal. Without it, a DETACHED ask's answer would
    // never wake the loop (no live worker transitions in sys-sessions).
    ("asks", crate::mailbox::sys_asks),
];

/// One sensor's outcome, for the run summary.
struct Reading {
    name: String,
    ok: bool,
    secs: u64,
    /// True when the sensor was NOT re-run this beat because its snapshot is
    /// still fresh under a declared `# looop:interval=N` cadence.
    skipped: bool,
}

/// A sensor script's declared cadence: the first `# looop:interval=<seconds>`
/// line, if any. A sensor with a declared interval is re-run only when its
/// existing snapshot is older than that — an expensive/rate-limited observer no
/// longer has to pay the full beat rate. No declaration = every beat (as before).
/// An EDITED script (mtime newer than its snapshot) always re-runs immediately,
/// so a fix never has to wait out the stale snapshot's cadence window.
fn declared_interval(script: &Path) -> Option<u64> {
    let text = fs::read_to_string(script).ok()?;
    for line in text.lines().take(20) {
        if let Some(rest) = line.trim().strip_prefix("# looop:interval=") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Modification time of `path`; `None` when absent/unreadable. Freshness
/// checks compare these `SystemTime`s DIRECTLY: an age-based comparison
/// (`modified().elapsed()`) fails on a future mtime (clock skew) and would
/// silently misread it as "old".
fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

impl Sensor {
    /// Snapshot basename (no extension): `sensor-<stem>` or `sys-<name>`.
    fn name(&self) -> String {
        match self {
            Sensor::User(p) => format!(
                "sensor-{}",
                p.file_stem().unwrap_or_default().to_string_lossy()
            ),
            Sensor::System { name, .. } => format!("sys-{name}"),
        }
    }

    /// Produce this sensor's snapshot into snap_dir; report ok + duration.
    fn sense(&self, paths: &Paths, snap_dir: &Path) -> Reading {
        let name = self.name();
        let t0 = Instant::now();
        let ok = match self {
            Sensor::User(script) => {
                let out = snap_dir.join(format!("{name}.json"));
                let err = snap_dir.join(format!("{name}.err"));
                // Declared cadence: keep the existing snapshot while it's fresh
                // — unless the SCRIPT was edited after the snapshot was taken
                // (an edited observer takes effect immediately, not after the
                // stale snapshot ages out). mtimes are compared DIRECTLY
                // (script_mtime > snapshot_mtime ⇒ rerun): age arithmetic
                // breaks on future mtimes (clock skew). A future SNAPSHOT
                // mtime counts as fresh (age 0).
                if let (Some(iv), Some(snap_m)) = (declared_interval(script), mtime(&out)) {
                    let fresh = snap_m.elapsed().map_or(true, |d| d.as_secs() < iv);
                    let edited = mtime(script).is_some_and(|m| m > snap_m);
                    if fresh && !edited {
                        return Reading {
                            name,
                            ok: true,
                            secs: 0,
                            skipped: true,
                        };
                    }
                }
                let rc = exec_sensor(script, &out, &err);
                // (A timed-out sensor — rc 124 — already carries its timeout
                // message in the normalized error snapshot written by
                // exec_sensor; nothing extra to append here.)
                // Drop the empty .err a successful sensor leaves behind.
                if fs::metadata(&err).is_ok_and(|m| m.len() == 0) {
                    let _ = fs::remove_file(&err);
                }
                rc == 0
            }
            // Virtual: a probe can't fail or hang, so there's no timeout/err path.
            Sensor::System { probe, .. } => {
                let body = probe(paths);
                // Rename-published like every watched write (probe_stamp's
                // invariant): a concurrent reader must never see partial JSON.
                let _ = util::write_atomic(
                    &snap_dir.join(format!("{name}.json")),
                    format!("{body}\n").as_bytes(),
                );
                true
            }
        };
        Reading {
            name,
            ok,
            secs: t0.elapsed().as_secs(),
            skipped: false,
        }
    }
}

/// Every sensor for this beat: the user `sensors/*.sh` followed by the fixed
/// system probes.
fn all_sensors(paths: &Paths) -> Vec<Sensor> {
    let mut v: Vec<Sensor> = sensor_scripts(paths)
        .into_iter()
        .map(Sensor::User)
        .collect();
    v.extend(
        SYSTEM_SENSORS
            .iter()
            .map(|&(name, probe)| Sensor::System { name, probe }),
    );
    v
}

/// System sensor: the live worker fleet (the pulse excludes itself, so it never
/// feeds its own wake signal). The wake SIGNAL is each worker's stable identity
/// — id/state/exit_code — plus a COARSE `health` classification, so a worker
/// starting, dying, or transitioning busy→stuck moves the world hash and wakes
/// a blocked `looop wait` / the tick exactly once per transition. The raw,
/// per-second numbers (idle/uptime/ask age) ride in `detail`, which the wake
/// hash ignores — they inform the decider without churning the hash every beat.
///
/// `health` (alive workers only):
///   • busy         — terminal output within the stuck threshold
///   • waiting-ask  — blocked on a pending ask (the human's turn; idle forever
///                    is legitimate)
///   • stuck        — no pending ask AND no output for ≥ threshold. There is
///                    no input channel to nudge a worker, so the only remedy
///                    is kill (+ re-dispatch if the goal still needs work).
/// Threshold: `LOOOP_WORKER_STUCK_SECS` (default 900).
/// Pure health classification for one worker (see [`sys_sessions`]). Unknown
/// idle (missing/undatable log) biases to busy: never call a worker stuck on
/// evidence we don't have.
fn worker_health(
    alive: bool,
    has_pending_ask: bool,
    idle_secs: Option<u64>,
    stuck_after: u64,
) -> &'static str {
    if !alive {
        "dead"
    } else if has_pending_ask {
        "waiting-ask"
    } else if idle_secs.is_some_and(|i| i >= stuck_after) {
        "stuck"
    } else {
        "busy"
    }
}

/// One worker's full health projection — the shared source for the
/// `sys-sessions` snapshot AND `looop worker list`.
pub(crate) struct WorkerHealth {
    pub id: String,
    pub state: String,
    pub alive: bool,
    pub exit_code: Option<i64>,
    /// busy / waiting-ask / stuck / dead — see [`worker_health`].
    pub health: &'static str,
    /// Seconds since the last PTY output (output.log mtime).
    pub idle_s: Option<u64>,
    /// Seconds since the session started.
    pub uptime_s: Option<u64>,
    /// Seconds the worker's pending ask has been waiting on the human.
    pub ask_age_s: Option<u64>,
    /// Post-condition verdict for a dead worker with a declared `verify`:
    /// Some(true)=pass, Some(false)=FAIL (exit status lied — treat as a failed
    /// worker), None = no verify declared or not yet run.
    pub verify: Option<bool>,
    /// Output tail of a FAILED verify (diagnostic for the tick prompt).
    pub verify_output: Option<String>,
}

/// The whole fleet's health, sorted by id (order-stable for the wake hash).
pub(crate) fn fleet_health(paths: &Paths) -> Vec<WorkerHealth> {
    let stuck_after = env_num("LOOOP_WORKER_STUCK_SECS", 900);
    let pending = crate::mailbox::pending(paths);
    let mut workers = session::list_workers(paths);
    workers.sort_by(|a, b| a.id.cmp(&b.id));
    workers
        .into_iter()
        .map(|s| {
            let ask = pending.iter().find(|a| a.worker == s.id);
            let idle = session::output_idle_secs(paths, &s.id);
            let verdict = crate::verify::result(paths, &s.id);
            WorkerHealth {
                health: worker_health(s.alive, ask.is_some(), idle, stuck_after),
                idle_s: idle,
                uptime_s: s.uptime_secs(),
                ask_age_s: ask.map(|a| crate::util::now_unix().saturating_sub(a.ts)),
                verify: verdict.as_ref().map(|v| v.ok),
                verify_output: verdict
                    .filter(|v| !v.ok && !v.output.trim().is_empty())
                    .map(|v| v.output),
                id: s.id,
                state: s.state,
                alive: s.alive,
                exit_code: s.exit_code,
            }
        })
        .collect()
}

fn sys_sessions(paths: &Paths) -> serde_json::Value {
    let fleet = fleet_health(paths);
    let mut signal: Vec<serde_json::Value> = Vec::new();
    let mut detail_workers = serde_json::Map::new();
    for w in &fleet {
        let mut sig = serde_json::json!({
            "id": w.id,
            "state": w.state,
            "exit_code": w.exit_code,
            "health": w.health,
        });
        if let Some(v) = w.verify {
            // In the SIGNAL half on purpose: a post-condition verdict must
            // change the world hash and wake the tick.
            sig["verify"] = serde_json::json!(if v { "pass" } else { "fail" });
        }
        signal.push(sig);
        let mut d = serde_json::Map::new();
        if let Some(o) = &w.verify_output {
            d.insert("verify_output".into(), serde_json::json!(o));
        }
        if let Some(i) = w.idle_s {
            d.insert("idle_s".into(), serde_json::json!(i));
        }
        if let Some(u) = w.uptime_s {
            d.insert("uptime_s".into(), serde_json::json!(u));
        }
        if let Some(a) = w.ask_age_s {
            d.insert("ask_age_s".into(), serde_json::json!(a));
        }
        // Undelivered steering (`looop tell`) — volatile detail: the human may
        // want to know a message hasn't been picked up yet, but delivery lag
        // must not wake the loop.
        let tells = crate::mailbox::pending_tells(paths, &w.id).len();
        if tells > 0 {
            d.insert("pending_tells".into(), serde_json::json!(tells));
        }
        detail_workers.insert(w.id.clone(), serde_json::Value::Object(d));
    }
    serde_json::json!({
        "signal": signal,
        "detail": { "count": fleet.len(), "workers": detail_workers },
    })
}

/// System sensor: live worker leases (claims/*.json). Stale claims are reaped
/// deterministically BEFORE sense, so every lease here is owned by a live worker
/// — a name listed is OWNED; the decider must not act on it itself. The lease
/// set IS the wake SIGNAL: a worker taking or releasing a task is a real
/// transition the decider may react to.
fn sys_claims(paths: &Paths) -> serde_json::Value {
    let store = crate::store::FileStore::new(paths);
    let leases: Vec<serde_json::Value> = store
        .list(&crate::store::Collection::Claims)
        .into_iter()
        .map(|name| {
            let claim: serde_json::Value = store
                .read(&crate::store::Key::Claim(name.clone()))
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({ "name": name, "claim": claim })
        })
        .collect();
    serde_json::json!({ "signal": leases })
}

/// System sensor: per-goal staleness, so the decider can avoid STARVING a goal
/// (fairness). For every current `goals/*.md` it reports how long since looop
/// last acted on that goal (`age_s`, null = never). The whole reading lives in
/// `.detail`: ages are time-volatile, so they must NOT feed the wake hash (an
/// empty `.signal` keeps sys-goals from ever waking the loop on its own — the
/// goal SET already wakes it via goals/*.md). When awake for other reasons, the
/// decider sees which goals it has been neglecting.
fn sys_goals(paths: &Paths) -> serde_json::Value {
    let store = crate::store::FileStore::new(paths);
    let activity: serde_json::Map<String, serde_json::Value> = store
        .read(&crate::store::Key::GoalActivity)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let now = util::now_unix();

    // store.list is already sorted.
    let ids: Vec<String> = store.list(&crate::store::Collection::Goals);

    let mut goals = serde_json::Map::new();
    for id in ids {
        let last = activity.get(&id).and_then(serde_json::Value::as_u64);
        let entry = match last {
            Some(ts) => serde_json::json!({
                "last_acted_unix": ts,
                "age_s": now.saturating_sub(ts),
            }),
            None => serde_json::json!({ "last_acted_unix": null, "age_s": null }),
        };
        goals.insert(id, entry);
    }
    serde_json::json!({ "signal": {}, "detail": { "goals": goals } })
}

/// Sorted list of `sensors/*.sh`.
pub fn sensor_scripts(paths: &Paths) -> Vec<PathBuf> {
    util::sorted_glob(&paths.sensors_dir(), "sh")
}

/// Run every sensor — user `sensors/*.sh` AND the virtual system probes — into
/// `snap_dir`. Caller is responsible for wiping the dir first (level-triggered).
/// When `verbose`, log each sensor + duration like a tick; otherwise stay quiet
/// (manual goal runs).
pub fn run_all(paths: &Paths, snap_dir: &Path, verbose: bool) {
    // Run due post-condition checks FIRST, so a worker that died since the
    // last beat gets its verdict recorded before sys-sessions snapshots it —
    // the fail lands in the same beat's world hash and wakes the tick.
    crate::verify::reconcile(paths);
    let sensors = all_sensors(paths);
    let total = sensors.len();

    // Prune snapshots that no live sensor owns (a deleted sensor's leftovers).
    // Snapshots are no longer wiped wholesale each beat — a fresh file under a
    // declared `# looop:interval` cadence must survive — so staleness is handled
    // by name instead: anything not in the current sensor set goes.
    let owned: std::collections::HashSet<String> = sensors.iter().map(Sensor::name).collect();
    for e in fs::read_dir(snap_dir).into_iter().flatten().flatten() {
        let p = e.path();
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if !owned.contains(&stem) {
            let _ = fs::remove_file(&p);
        }
    }

    // Per-sensor lines are machine granularity — only the JSON stream gets them.
    // The human pulse stream gets ONE summary line (below), so a healthy fleet of
    // sensors doesn't drown the decisions a watcher actually cares about.
    let json = util::is_json();
    let t0_all = Instant::now();

    // Sensors are independent by construction (each owns its snapshot file), so
    // run them CONCURRENTLY: the beat's sense phase costs max(sensor latency),
    // not the sum — one slow network observer no longer stalls the whole pulse.
    // Events are emitted after the join so the log stream stays ordered.
    let readings: Vec<Reading> = std::thread::scope(|scope| {
        let handles: Vec<_> = sensors
            .iter()
            .map(|s| (s.name(), scope.spawn(move || s.sense(paths, snap_dir))))
            .collect();
        // Join per handle: a PANICKING sensor thread becomes a failed reading
        // for THAT sensor — it must never discard its siblings' outcomes.
        handles
            .into_iter()
            .map(|(name, h)| {
                h.join().unwrap_or(Reading {
                    name,
                    ok: false,
                    secs: 0,
                    skipped: false,
                })
            })
            .collect()
    });

    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for r in &readings {
        if r.skipped {
            skipped += 1;
        }
        if r.ok {
            ok += 1;
            if verbose && json && !r.skipped {
                util::event(
                    Level::Ok,
                    "sense.ok",
                    &format!("{} ({}s)", r.name, r.secs),
                    &[
                        ("sensor", serde_json::json!(r.name)),
                        ("secs", serde_json::json!(r.secs)),
                    ],
                );
            }
        } else {
            if verbose && json {
                util::event(
                    Level::Error,
                    "sense.fail",
                    &format!(
                        "{} failed ({}s) — see snapshots/{}.err",
                        r.name, r.secs, r.name
                    ),
                    &[
                        ("sensor", serde_json::json!(r.name)),
                        ("secs", serde_json::json!(r.secs)),
                    ],
                );
            }
            failed.push(r.name.clone());
        }
    }

    // The summary: a single dim heartbeat line when all is well, a red line that
    // names the offenders when not. (In JSON mode this rides alongside the
    // per-sensor events as a `sense` aggregate.)
    if verbose && total > 0 {
        let secs = t0_all.elapsed().as_secs();
        let fields = [
            ("ok", serde_json::json!(ok)),
            ("total", serde_json::json!(total)),
            ("skipped", serde_json::json!(skipped)),
            ("failed", serde_json::json!(failed)),
            ("secs", serde_json::json!(secs)),
        ];
        if failed.is_empty() {
            let cadence = if skipped > 0 {
                format!(" · {skipped} fresh (cadence)")
            } else {
                String::new()
            };
            util::event(
                Level::Info,
                "sense",
                &format!("{ok} sensors ok ({secs}s){cadence}"),
                &fields,
            );
        } else {
            util::event(
                Level::Error,
                "sense",
                &format!("{ok}/{total} sensors ok · failed: {}", failed.join(", ")),
                &fields,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensor_name_is_kind_prefixed() {
        let user = Sensor::User(PathBuf::from("/x/sensors/today.sh"));
        assert_eq!(user.name(), "sensor-today");
        let sys = Sensor::System {
            name: "sessions",
            probe: sys_sessions,
        };
        assert_eq!(sys.name(), "sys-sessions");
    }

    #[test]
    fn worker_health_classifies_the_four_states() {
        // Dead wins over everything.
        assert_eq!(worker_health(false, true, Some(9999), 900), "dead");
        // A pending ask is the human's turn — legitimate idle, however long.
        assert_eq!(worker_health(true, true, Some(9999), 900), "waiting-ask");
        // Silent past the threshold with no ask — stuck.
        assert_eq!(worker_health(true, false, Some(900), 900), "stuck");
        assert_eq!(worker_health(true, false, Some(899), 900), "busy");
        // Unknown idle biases to busy (never stuck on missing evidence).
        assert_eq!(worker_health(true, false, None, 900), "busy");
    }

    #[test]
    fn sys_claims_signal_lists_live_leases_sorted() {
        let p = Paths::temp();
        fs::create_dir_all(p.claims_dir()).unwrap();
        fs::write(
            p.claims_dir().join("repo-b.json"),
            br#"{"session":"w2","name":"repo-b"}"#,
        )
        .unwrap();
        fs::write(
            p.claims_dir().join("repo-a.json"),
            br#"{"session":"w1","name":"repo-a"}"#,
        )
        .unwrap();

        let v = sys_claims(&p);
        let leases = v.get("signal").and_then(|s| s.as_array()).unwrap();
        assert_eq!(leases.len(), 2);
        // Sorted by file name so the snapshot — and the world hash — is stable.
        assert_eq!(leases[0]["name"], "repo-a");
        assert_eq!(leases[1]["name"], "repo-b");
        assert_eq!(leases[0]["claim"]["session"], "w1");
    }

    #[test]
    fn sys_goals_reports_age_per_goal_and_never_for_unacted() {
        let p = Paths::temp();
        fs::create_dir_all(p.goals_dir()).unwrap();
        fs::write(p.goals_dir().join("triage.md"), b"goal: triage\n").unwrap();
        fs::write(p.goals_dir().join("ship.md"), b"goal: ship\n").unwrap();
        // triage was acted on a while ago; ship never.
        let then = util::now_unix().saturating_sub(120);
        fs::write(p.goal_activity(), format!(r#"{{"triage":{then}}}"#)).unwrap();

        let v = sys_goals(&p);
        // ages are volatile -> only in .detail, never in the wake signal.
        assert_eq!(v["signal"], serde_json::json!({}));
        let goals = &v["detail"]["goals"];
        assert!(
            goals["triage"]["age_s"].as_u64().unwrap() >= 120,
            "acted goal reports a real age"
        );
        assert!(
            goals["ship"]["age_s"].is_null(),
            "never-acted goal reports null age"
        );
        // An empty signal must never wake the loop.
        assert_eq!(crate::worldhash::wake_signal(v), serde_json::json!({}));
    }

    #[test]
    fn declared_interval_parses_the_cadence_comment() {
        let p = Paths::temp();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        let s = p.sensors_dir().join("gh.sh");
        fs::write(&s, "#!/bin/sh\n# looop:interval=300\necho '{}'\n").unwrap();
        assert_eq!(declared_interval(&s), Some(300));
        let t = p.sensors_dir().join("fast.sh");
        fs::write(&t, "#!/bin/sh\necho '{}'\n").unwrap();
        assert_eq!(declared_interval(&t), None, "no declaration = every beat");
    }

    #[test]
    fn fresh_snapshot_is_kept_under_a_declared_cadence() {
        let p = Paths::temp();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        let script = p.sensors_dir().join("slow.sh");
        fs::write(
            &script,
            "#!/bin/sh\n# looop:interval=3600\necho '{\"signal\":{\"ran\":true}}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let sensor = Sensor::User(script);
        let out = p.snapshots_dir().join("sensor-slow.json");

        // No snapshot yet: runs.
        let r1 = sensor.sense(&p, &p.snapshots_dir());
        assert!(r1.ok && !r1.skipped, "first sense runs the script");
        let body1 = fs::read_to_string(&out).unwrap();

        // Snapshot fresh under the 1h cadence: kept, not re-run.
        let r2 = sensor.sense(&p, &p.snapshots_dir());
        assert!(r2.ok && r2.skipped, "fresh snapshot is kept (cadence)");
        assert_eq!(fs::read_to_string(&out).unwrap(), body1);

        // An EDITED script (mtime newer than its snapshot) re-runs immediately
        // instead of waiting out the cadence window. Simulate by aging the
        // snapshot's mtime behind the script's.
        let f = fs::File::options().write(true).open(&out).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
            .unwrap();
        drop(f);
        let r3 = sensor.sense(&p, &p.snapshots_dir());
        assert!(
            r3.ok && !r3.skipped,
            "script newer than snapshot: cadence must not keep the stale reading"
        );
    }

    #[test]
    fn run_all_prunes_snapshots_no_live_sensor_owns() {
        let p = Paths::temp();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        // A leftover from a since-deleted sensor.
        let stale = p.snapshots_dir().join("sensor-gone.json");
        fs::write(&stale, "{}").unwrap();

        run_all(&p, &p.snapshots_dir(), false);
        assert!(!stale.exists(), "unowned snapshot pruned");
        assert!(
            p.snapshots_dir().join("sys-sessions.json").is_file(),
            "system snapshots written"
        );
        assert!(
            p.snapshots_dir().join("sys-schedules.json").is_file(),
            "sys-schedules registered"
        );
    }

    #[test]
    fn sys_claims_empty_when_no_dir() {
        let p = Paths::temp();
        let v = sys_claims(&p);
        assert_eq!(v["signal"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn failed_user_sensor_becomes_error_snapshot() {
        let p = Paths::temp();
        let snap = p.snapshots_dir();
        fs::create_dir_all(&snap).unwrap();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        let script = p.sensors_dir().join("boom.sh");
        fs::write(&script, "#!/usr/bin/env bash\necho 'kaboom' >&2\nexit 3\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(&script).unwrap().permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&script, perm).unwrap();
        }

        let r = Sensor::User(script).sense(&p, &snap);
        assert!(!r.ok, "a non-zero exit is a failure");
        let body = fs::read_to_string(snap.join("sensor-boom.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // The failure is a normalized {signal,detail} object the decider can read.
        assert_eq!(v["signal"]["error"], serde_json::json!(true));
        assert_eq!(v["signal"]["exit_code"], serde_json::json!(3));
        assert!(
            v["detail"]["stderr"].as_str().unwrap().contains("kaboom"),
            "stderr tail reaches the prompt"
        );
        // Only .signal feeds the world hash: a stable break wakes the loop once,
        // not on every volatile stderr change.
        assert_eq!(
            crate::worldhash::wake_signal(v.clone()),
            serde_json::json!({ "error": true, "exit_code": 3 })
        );
    }

    #[test]
    #[cfg(unix)]
    fn unspawnable_sensor_leaves_an_error_snapshot_not_an_empty_one() {
        // File::create(out) truncates the PREVIOUS snapshot before spawn — a
        // spawn failure (no exec bit here; a bogus shebang behaves the same)
        // must therefore write the normalized error blob, never early-return
        // with an empty snapshot the decider can't explain.
        let p = Paths::temp();
        let snap = p.snapshots_dir();
        fs::create_dir_all(&snap).unwrap();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        let script = p.sensors_dir().join("noexec.sh");
        fs::write(&script, "#!/bin/sh\necho '{}'\n").unwrap();
        // Deliberately NOT executable.
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        }
        // A previous beat's healthy snapshot that must not survive as-is
        // (stale) nor be silently blanked (empty).
        let out = snap.join("sensor-noexec.json");
        fs::write(&out, "{\"signal\":{\"ok\":true}}\n").unwrap();

        let r = Sensor::User(script).sense(&p, &snap);
        assert!(!r.ok, "an unspawnable sensor is a failure");
        let body = fs::read_to_string(&out).unwrap();
        assert!(!body.trim().is_empty(), "snapshot must never be left empty");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["signal"]["error"], serde_json::json!(true));
        assert_eq!(v["signal"]["exit_code"], serde_json::json!(1));
        assert!(
            v["detail"]["message"]
                .as_str()
                .unwrap()
                .contains("spawn failed"),
            "the spawn failure reaches the prompt: {body}"
        );
    }

    #[test]
    fn oversized_sensor_output_has_a_size_stable_wake_signal() {
        // The over-cap replacement blob must not carry the VOLATILE byte count
        // in its wake signal: two oversized readings of different sizes must
        // produce the SAME signal, or the world hash flaps on every beat.
        let p = Paths::temp();
        let snap = p.snapshots_dir();
        fs::create_dir_all(&snap).unwrap();
        fs::create_dir_all(p.sensors_dir()).unwrap();

        let _g = crate::util::test_env_lock();
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.0 {
                    Some(v) => unsafe { std::env::set_var("LOOOP_SENSOR_MAX_BYTES", v) },
                    None => unsafe { std::env::remove_var("LOOOP_SENSOR_MAX_BYTES") },
                }
            }
        }
        let _r = Restore(std::env::var_os("LOOOP_SENSOR_MAX_BYTES"));
        unsafe { std::env::set_var("LOOOP_SENSOR_MAX_BYTES", "16") };

        let mut signals = Vec::new();
        for (name, n) in [("big-a", 100usize), ("big-b", 5000usize)] {
            let script = p.sensors_dir().join(format!("{name}.sh"));
            fs::write(&script, format!("#!/bin/sh\nprintf 'x%.0s' $(seq {n})\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let r = Sensor::User(script).sense(&p, &snap);
            assert!(r.ok, "over-cap is a normalized reading, not a failure");
            let body = fs::read_to_string(snap.join(format!("sensor-{name}.json"))).unwrap();
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            // Volatile size lives in .detail only.
            assert!(v["detail"]["bytes"].as_u64().unwrap() > 16);
            assert_eq!(v["signal"]["error"], serde_json::json!("too-large"));
            signals.push(crate::worldhash::wake_signal(v).to_string());
        }
        assert_eq!(
            signals[0], signals[1],
            "differing oversizes must not move the wake signal"
        );
    }

    #[test]
    fn hung_sensor_is_killed_by_the_in_process_timeout() {
        let p = Paths::temp();
        let snap = p.snapshots_dir();
        fs::create_dir_all(&snap).unwrap();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        let script = p.sensors_dir().join("hang.sh");
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        // 1s budget — no external `timeout`/`gtimeout` binary involved.
        // set_var is process-global: serialize against other env-mutating
        // tests and restore the knob even if an assert below panics.
        let _g = crate::util::test_env_lock();
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.0 {
                    Some(v) => unsafe { std::env::set_var("LOOOP_SENSOR_TIMEOUT", v) },
                    None => unsafe { std::env::remove_var("LOOOP_SENSOR_TIMEOUT") },
                }
            }
        }
        let _r = Restore(std::env::var_os("LOOOP_SENSOR_TIMEOUT"));
        unsafe { std::env::set_var("LOOOP_SENSOR_TIMEOUT", "1") };
        let r = Sensor::User(script).sense(&p, &snap);

        assert!(!r.ok, "a timed-out sensor is a failure");
        let body = fs::read_to_string(snap.join("sensor-hang.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["signal"]["exit_code"], serde_json::json!(124));
        assert!(
            v["detail"]["message"]
                .as_str()
                .unwrap()
                .contains("timed out"),
            "timeout reaches the prompt via the snapshot: {body}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn signal_killed_sensor_reports_128_plus_signo() {
        let p = Paths::temp();
        let snap = p.snapshots_dir();
        fs::create_dir_all(&snap).unwrap();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        let script = p.sensors_dir().join("suicide.sh");
        // The script kills ITSELF with SIGTERM — a stand-in for an external
        // kill (OOM, operator TERM) that leaves no exit code.
        fs::write(&script, "#!/usr/bin/env bash\nkill -TERM $$\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let r = Sensor::User(script).sense(&p, &snap);
        assert!(!r.ok, "a signal death is a failure");
        let body = fs::read_to_string(snap.join("sensor-suicide.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // SIGTERM is 15 on every unix we target → 128+15.
        assert_eq!(
            v["signal"]["exit_code"],
            serde_json::json!(143),
            "signal deaths report the conventional 128+signo, not a generic 1: {body}"
        );
        assert!(
            v["detail"]["message"]
                .as_str()
                .unwrap()
                .contains("killed by signal 15"),
            "the blob names the signal so the decider can tell an external kill \
             from a script bug: {body}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn plain_exit_above_128_is_not_misreported_as_a_signal_death() {
        // A script that itself `exit 130`s has a real exit CODE — the blob
        // must not claim "killed by signal 2" just because 130 > 128.
        let p = Paths::temp();
        let snap = p.snapshots_dir();
        fs::create_dir_all(&snap).unwrap();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        let script = p.sensors_dir().join("exit130.sh");
        fs::write(&script, "#!/usr/bin/env bash\nexit 130\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let r = Sensor::User(script).sense(&p, &snap);
        assert!(!r.ok);
        let body = fs::read_to_string(snap.join("sensor-exit130.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["signal"]["exit_code"], serde_json::json!(130));
        assert!(
            !v["detail"]["message"]
                .as_str()
                .unwrap()
                .contains("killed by signal"),
            "an ordinary exit code above 128 is a script failure, not a signal death: {body}"
        );
    }

    #[test]
    fn successful_sensor_is_rename_published_leaving_no_tmp() {
        let p = Paths::temp();
        let snap = p.snapshots_dir();
        fs::create_dir_all(&snap).unwrap();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        let script = p.sensors_dir().join("ok.sh");
        fs::write(
            &script,
            "#!/usr/bin/env bash\necho '{\"signal\":{\"n\":1}}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let r = Sensor::User(script).sense(&p, &snap);
        assert!(r.ok);
        let body = fs::read_to_string(snap.join("sensor-ok.json")).unwrap();
        serde_json::from_str::<serde_json::Value>(&body).expect("published snapshot is whole JSON");
        assert!(
            !snap.join("sensor-ok.json.tmp").exists(),
            "the capture temp is consumed by the publishing rename — watchers \
             only ever see the snapshot appear atomically"
        );
    }

    #[test]
    fn future_snapshot_mtime_still_counts_as_fresh() {
        let p = Paths::temp();
        fs::create_dir_all(p.sensors_dir()).unwrap();
        fs::create_dir_all(p.snapshots_dir()).unwrap();
        let script = p.sensors_dir().join("skewed.sh");
        fs::write(
            &script,
            "#!/bin/sh\n# looop:interval=3600\necho '{\"signal\":{}}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let sensor = Sensor::User(script);
        let out = p.snapshots_dir().join("sensor-skewed.json");
        let r1 = sensor.sense(&p, &p.snapshots_dir());
        assert!(r1.ok && !r1.skipped);

        // Clock skew: snapshot mtime 2 minutes in the FUTURE. An age-based
        // check would see "no age" and re-run; the mtime comparison keeps it.
        let f = fs::File::options().write(true).open(&out).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(120))
            .unwrap();
        drop(f);
        let r2 = sensor.sense(&p, &p.snapshots_dir());
        assert!(
            r2.ok && r2.skipped,
            "future snapshot mtime must read as fresh, not as stale"
        );
    }

    #[test]
    fn system_sensor_sense_writes_snapshot() {
        let p = Paths::temp();
        let snap = p.snapshots_dir();
        fs::create_dir_all(&snap).unwrap();
        let s = Sensor::System {
            name: "claims",
            probe: sys_claims,
        };
        let r = s.sense(&p, &snap);
        assert!(r.ok);
        assert_eq!(r.name, "sys-claims");
        assert!(snap.join("sys-claims.json").is_file());
    }
}
