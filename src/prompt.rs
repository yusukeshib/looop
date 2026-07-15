//! DECIDE — assemble the one-tick prompt: the PLAYBOOK, goals, sensor readings,
//! pending asks, worker sessions and recent journal. The instruction text is
//! fixed; only the data dir (stable per install) is substituted into it.
//!
//! ORDERING (prompt-cache friendly, deliberate): sections ride from most stable
//! to most volatile — INSTRUCTIONS + CONSTITUTION (static bytes), PLAYBOOK +
//! GOALS (change on edits), then snapshots/asks/journal, and LAST the current
//! time + closing instruction. Provider prompt caching hits the longest
//! byte-identical prefix, so the per-beat-changing time must never sit above
//! the static sections — keep anything time-varying at the tail.

use crate::mailbox;
use crate::paths::Paths;
use crate::session;
use crate::store::{Collection, FileStore, Key, StateStore};
use crate::util;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// The immutable minimal norms. Unlike the PLAYBOOK (which the AI may rewrite via
/// `write_playbook`), this lives in the binary and CANNOT be edited by any move.
/// It is injected ahead of the PLAYBOOK and OVERRIDES it on any conflict, so the
/// loop can't weaken its own irreversibility/run_shell guardrails by grooming the
/// PLAYBOOK. The PLAYBOOK is operational tuning UNDER this constitution.
const CONSTITUTION: &str = r#"These norms are FIXED (compiled into looop). They override the PLAYBOOK on any
conflict, and no move — including write_playbook — can remove or weaken them.

1. NEVER do irreversible things automatically: merging, deploying, deleting data,
   closing issues, publishing public comments, force-pushing, sending money. For
   any of these: PREPARE fully, then start a worker that does the work and, at the
   point of no return, runs `looop ask` to WAIT for a human decision. The HUMAN
   decides irreversible moves — never you.
2. run_shell is ONE ad-hoc, REVERSIBLE, NON-DESTRUCTIVE command only (a query, a
   draft, a read). Anything irreversible/destructive (rule 1) must NOT go through
   run_shell; it must go through a worker that asks the human first. When unsure,
   treat it as irreversible.
3. SINGLE-WRITER POLICY FILES: only the pulse (this tick) writes PLAYBOOK.md,
   goals/ and sensors/, and only via the typed actions below — never by editing
   files directly.
4. ASK, DON'T GUESS: when you lack the information or authority to choose safely,
   surface it through a worker that runs `looop ask` (the human answers it)
   rather than guess. Asking is cheaper than a wrong irreversible move.
5. write_playbook may tune priorities and add rules, but MUST keep these five
   norms intact. The PLAYBOOK refines judgment beneath them; it never overrides
   them.
"#;

const INSTRUCTIONS: &str = r#"You are "looop", an autonomous personal operations agent. This is one tick of a
loop; your process is disposable. Your working directory is the loop's DATA dir
(__DATA__).

A fixed CONSTITUTION (below, compiled into looop) sets the non-negotiable norms.
It OVERRIDES the PLAYBOOK on any conflict, and no move can weaken it.

Read the PLAYBOOK, goals, sensor readings, pending asks and sessions below, then
decide the SINGLE most important move — and stop.

You do NOT perform the move yourself. You EMIT it: write exactly ONE JSON object
describing your chosen move to `.decision.json` in your working directory. looop
— not you — then executes it. This is what guarantees one move per tick and lets
looop gate risky actions. So:
  • Do NOT edit goals/, sensors/, PLAYBOOK.md or journal.md directly.
  • Do NOT run side-effecting commands yourself. Read-only inspection is
    allowed but rarely needed (see the note above the NOW section); the MOVE
    itself must be the JSON action below.
  • Emit exactly one object. If nothing needs doing, emit the `noop` action.

Pick exactly ONE `action` and fill its fields:

  {"action":"noop","reason":"why nothing is the right move"}

  {"action":"run_shell","cmd":"<one shell command>","reason":"..."}
     One ad-hoc, REVERSIBLE side-effecting command (a gh query, posting a
     draft…); looop runs it in the data dir. Never irreversible (merge / deploy /
     delete / public comment) — for those, start a worker that prepares it and
     asks the human (the worker runs `looop ask`).

  {"action":"write_goal","id":"<name>","body":"<full goals/<name>.md contents>"}
     Create or replace a goal — desired state, declarative; evaluated every tick,
     never executed.

  {"action":"archive_goal","id":"<name>"}   move goals/<name>.md into archive/

  {"action":"write_sensor","name":"<name>","script":"<full sensors/<name>.sh>"}
     A new/updated observer. It must print ONE small NORMALIZED JSON object to
     stdout (capped ~8KB). Split volatile fields out so noise doesn't wake the
     loop: {"signal":{…only state that should trigger a move…},
     "detail":{…counts/timestamps/context…}} — only .signal feeds the
     change-detection hash; the whole object still reaches this prompt.

  {"action":"start_worker","id":"<goal-name>","prompt":"<detailed worker brief>"}
     Spawn an agent for hands-on, multi-step work. <id> matches the goal file.
     The worker starts in the data dir; if its task edits CODE, tell it to make
     its OWN sandbox first (a git worktree) and cd in —
     never edit code in the data dir. A worker that needs a human decision runs
     `looop ask <id> --prompt "…"` and BLOCKS until the human answers — prefer
     one worker per goal over spawning a second for the same goal.

  {"action":"kill_worker","id":"<worker-id>","reason":"..."}
     Terminate a live worker. Workers have NO input channel (no terminal a
     human or you can type into) — the ask/answer mailbox is their only I/O —
     so a worker that is alive, NOT waiting on an ask, and silent past the
     stuck threshold (sys-sessions health: "stuck") cannot be nudged, only
     killed. If its goal still needs the work, re-dispatch a FRESH worker on a
     later beat (it can read reports/ for the prior context). NEVER kill a
     "waiting-ask" worker — it is the human's turn, and killing it strands
     their eventual answer.

  {"action":"write_playbook","body":"<full PLAYBOOK.md contents>"}
     Change your own judgment / guardrails. Deliberate — only harden a drift into
     a rule once it actually hurts.

Every action ALSO takes:
  "journal": "<one line: what you did and why>"  — looop appends it ALREADY
     timestamped, so do NOT restate the date or time inside it (no "02:31 AM,").
  "next_interval_s": <int>  — OPTIONAL one-shot cadence nudge (clamped 5..3600):
     tighten when a backlog is piling up, widen when it's been quiet a long while.
     It ALSO forces the next beat to re-decide even if nothing in the world
     changed — use it for a time-based follow-up ("re-check in N seconds"), since
     an unchanged world otherwise skips the AI entirely.

PENDING ASKS are asks raised via `looop ask` and not yet answered. They are
NOT yours to answer — the human answers them out of band. Each ask is tagged
LIVE or STRANDED:
  • LIVE — the asking worker is still alive and blocked on the human. Do NOT
    re-dispatch or duplicate work it is already blocked on; the human answers it
    out of band and the worker resumes.
  • STRANDED — the asking worker is DEAD (exited/crashed/killed, e.g. after a
    reboot). Its answer can NEVER be delivered — no live process will consume
    it, so a human answer is inert. A stranded ask is NOT a reason to noop: if
    the underlying goal still needs the work, re-dispatch a FRESH worker for it
    (it can read the prior `reports/*.md` for context and re-raise the ask as a
    LIVE worker). Treat STRANDED asks as work to resume, not as blockers.

Two of the SENSOR READINGS are looop's OWN state (system sensors, not
sensors/*.sh):
  • sys-sessions — the live worker fleet, each tagged with a health reading:
      busy         actively producing terminal output — leave it alone
      waiting-ask  blocked on a pending ask — the HUMAN's turn; idle forever is
                   legitimate, never kill it
      stuck        alive, no ask, no output past the threshold — unreachable
                   (workers have no input channel); kill_worker is the only
                   remedy, then re-dispatch fresh if the goal still needs work
    .detail.workers[id] carries the raw numbers (idle_s / uptime_s / ask_age_s).
    Prefer steering work through ONE worker per goal over spawning a SECOND one
    for the same goal.
  • sys-goals — per-goal staleness (.detail.goals[id].age_s = seconds since you
    last acted on that goal; null = never). FAIRNESS: you pick ONE move per beat,
    so a constantly-changing goal can starve the rest. When several goals are
    ready and roughly comparable, prefer the one you've neglected longest.

The material below is your decision input. Decide from it alone whenever it
suffices; run read-only inspection ONLY when a fact you genuinely need is
missing from it — one narrow query, not a survey. Most beats need zero tools:
read, emit the JSON move, stop.

"#;

/// The single most-neglected goal: the top-level `goals/*.md` looop has gone
/// longest without acting on (a goal never acted on outranks any acted one).
/// `None` when there are no goals. Computed by looop — not left to the AI to scan
/// — so the fairness nudge names a concrete goal the decider must justify
/// skipping (RULE: one move/beat can otherwise starve the quiet goals).
fn most_neglected_goal(paths: &Paths) -> Option<String> {
    let store = FileStore::new(paths);
    let activity: serde_json::Map<String, serde_json::Value> = store
        .read(&Key::GoalActivity)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // store.list is already sorted (deterministic tie-break).
    // last-acted unix; never-acted => 0 (oldest possible) => ranked most neglected.
    store
        .list(&Collection::Goals)
        .into_iter()
        .min_by_key(|id| activity.get(id).and_then(|v| v.as_u64()).unwrap_or(0))
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

pub fn build_prompt(paths: &Paths, snap_dir: &Path) -> String {
    let mut out = String::new();

    let instr = INSTRUCTIONS.replace("__DATA__", &paths.data_dir.to_string_lossy());
    out.push_str(&instr);

    // CONSTITUTION (immutable, binary-embedded) — ahead of the PLAYBOOK and
    // overriding it. The AI can rewrite the PLAYBOOK but never this.
    out.push_str("=== CONSTITUTION (immutable — overrides PLAYBOOK) ===\n");
    out.push_str(CONSTITUTION);
    out.push('\n');

    // PLAYBOOK.
    let store = FileStore::new(paths);
    out.push_str("=== PLAYBOOK ===\n");
    out.push_str(&store.read(&Key::Playbook).unwrap_or_default());
    out.push('\n');

    // GOALS.
    out.push_str("\n=== GOALS ===\n");
    let goals = store.list(&Collection::Goals);
    if goals.is_empty() {
        out.push_str("(no goals yet)\n");
    } else {
        for id in goals {
            let _ = writeln!(out, "--- {id}.md");
            out.push_str(&store.read(&Key::Goal(id)).unwrap_or_default());
            out.push('\n');
        }
    }

    // SENSOR READINGS.
    out.push_str("\n=== SENSOR READINGS ===\n");
    for o in util::sorted_glob(snap_dir, "json") {
        let fname = o
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if !(fname.starts_with("sensor-") || fname.starts_with("sys-")) {
            continue;
        }
        let _ = writeln!(out, "--- {fname}");
        out.push_str(&fs::read_to_string(&o).unwrap_or_default());
        out.push('\n');
    }

    // PENDING ASKS — workers blocked waiting for a HUMAN answer. The decider sees
    // them so it doesn't re-dispatch work that's already waiting on the human.
    out.push_str("\n=== PENDING ASKS (waiting on the human — not yours to answer) ===\n");
    let asks = mailbox::pending(paths);
    if asks.is_empty() {
        out.push_str("(none)\n");
    } else {
        // A pending ask only blocks a LIVE worker; a dead worker's ask is
        // STRANDED (its answer can never be delivered) and must not suppress
        // re-dispatch. Collect the alive worker ids once to tag each ask.
        let alive: std::collections::HashSet<String> = session::list_workers(paths)
            .into_iter()
            .filter(|s| s.alive)
            .map(|s| s.id)
            .collect();
        for a in asks {
            let tag = if alive.contains(&a.worker) {
                "LIVE — blocked on human"
            } else {
                "STRANDED — worker DEAD, answer inert; re-dispatch a fresh worker"
            };
            let _ = writeln!(out, "--- {} (worker {} — {tag})", a.id, a.worker);
            let _ = writeln!(out, "{}", a.prompt);
            if !a.reference.is_empty() {
                let _ = writeln!(out, "reference: {}", a.reference);
            }
            if !a.options.is_empty() {
                let _ = writeln!(out, "options: {}", a.options.join(", "));
            }
        }
    }

    // FAIRNESS (computed by looop, not left to the AI to eyeball sys-goals).
    if let Some(g) = most_neglected_goal(paths) {
        out.push_str("\n=== FAIRNESS (computed by looop) ===\n");
        let _ = writeln!(
            out,
            "Most neglected goal: `{g}`. You make ONE move per beat, so a loud,\n\
             constantly-changing goal can starve the quiet ones. If `{g}` is READY and\n\
             not clearly lower priority than the alternatives, prefer it THIS beat.\n\
             Otherwise, say in your `journal` why you're skipping it."
        );
    }

    // RECENT JOURNAL.
    out.push_str("\n=== RECENT JOURNAL ===\n");
    match store.read(&Key::Journal) {
        Some(j) if !j.is_empty() => {
            out.push_str(&tail_lines(&j, 20));
            out.push('\n');
        }
        _ => out.push_str("(empty)\n"),
    }

    // NOW (volatile tail — keep LAST). The current time is the only instruction
    // field that changes every beat; anchoring it (and the closing instruction)
    // at the very END keeps the long prefix above byte-identical across beats so
    // provider prompt caching can hit it. Do not move time-varying text above
    // the stable sections.
    out.push_str("\n=== NOW ===\n");
    let _ = writeln!(
        out,
        "Current local time: {}.",
        util::date_fmt("%Y-%m-%d %H:%M %Z")
    );
    out.push_str("\nWrite your single JSON object to `.decision.json` now, then stop.\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Paths {
        let p = Paths::temp();
        fs::create_dir_all(p.goals_dir()).unwrap();
        fs::create_dir_all(p.claims_dir()).unwrap();
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::write(p.playbook(), b"PB RULES\n").unwrap();
        fs::write(p.goals_dir().join("triage.md"), b"triage the inbox\n").unwrap();
        p
    }

    #[test]
    fn build_prompt_has_all_sections() {
        let p = fixture();
        let out = build_prompt(&p, &p.snapshots_dir());
        for marker in [
            "=== CONSTITUTION (immutable — overrides PLAYBOOK) ===",
            "=== PLAYBOOK ===",
            "=== GOALS ===",
            "=== SENSOR READINGS ===",
            "=== PENDING ASKS",
            "=== RECENT JOURNAL ===",
            "=== NOW ===",
        ] {
            assert!(out.contains(marker), "missing section: {marker}");
        }
        assert!(
            out.find("=== CONSTITUTION").unwrap() < out.find("=== PLAYBOOK").unwrap(),
            "constitution must precede the playbook"
        );
        assert!(
            out.contains("no move — including write_playbook — can remove or weaken them"),
            "constitution states its own immutability"
        );
        assert!(out.contains("PB RULES"), "playbook body inlined");
        assert!(out.contains("triage the inbox"), "goal body inlined");
    }

    #[test]
    fn volatile_time_rides_at_the_tail_for_prompt_cache_stability() {
        // Provider prompt caching hits the longest byte-identical PREFIX, so the
        // per-beat-changing time must sit BELOW every stable section, and the
        // closing instruction must be the very last line.
        let p = fixture();
        let out = build_prompt(&p, &p.snapshots_dir());
        assert!(!out.contains("__NOW__"), "no leftover placeholder");
        let now_pos = out.find("Current local time:").expect("time present");
        for stable in [
            "=== CONSTITUTION",
            "=== PLAYBOOK ===",
            "=== GOALS ===",
            "=== SENSOR READINGS ===",
            "=== PENDING ASKS",
            "=== RECENT JOURNAL ===",
        ] {
            assert!(
                out.find(stable).unwrap() < now_pos,
                "{stable} must precede the volatile time tail"
            );
        }
        assert!(
            out.trim_end().ends_with("then stop."),
            "closing instruction is the last line"
        );
    }

    #[test]
    fn prompt_prefix_before_now_is_stable_across_builds() {
        // Two builds over the same world must produce a byte-identical prefix
        // above the NOW tail — that identity is what makes prompt caching work.
        let p = fixture();
        let a = build_prompt(&p, &p.snapshots_dir());
        let b = build_prompt(&p, &p.snapshots_dir());
        let prefix = |s: &str| s.split("=== NOW ===").next().unwrap().to_string();
        assert_eq!(prefix(&a), prefix(&b), "stable sections must not drift");
    }

    #[test]
    fn pending_asks_are_surfaced() {
        let p = fixture();
        fs::write(
            p.asks_dir().join("triage-1.json"),
            serde_json::json!({"id":"triage-1","worker":"triage","prompt":"merge PR?","ts":1})
                .to_string(),
        )
        .unwrap();
        let out = build_prompt(&p, &p.snapshots_dir());
        assert!(out.contains("triage-1"));
        assert!(out.contains("merge PR?"));
    }

    #[test]
    fn dead_worker_ask_is_tagged_stranded_not_a_blocker() {
        // The reboot deadlock: an ask whose worker has no live session must be
        // surfaced as STRANDED so the decider re-dispatches instead of noop'ing
        // forever waiting for an answer no live process can consume.
        let p = fixture(); // no live sessions registered
        fs::write(
            p.asks_dir().join("triage-1.json"),
            serde_json::json!({"id":"triage-1","worker":"triage","prompt":"merge PR?","ts":1})
                .to_string(),
        )
        .unwrap();
        let out = build_prompt(&p, &p.snapshots_dir());
        // The ask line for the dead worker is tagged STRANDED, not LIVE.
        let ask_line = out
            .lines()
            .find(|l| l.contains("triage-1") && l.contains("worker triage"))
            .expect("ask line present");
        assert!(
            ask_line.contains("STRANDED"),
            "dead-worker ask must be STRANDED: {ask_line}"
        );
        assert!(
            !ask_line.contains("LIVE"),
            "dead-worker ask must not be LIVE: {ask_line}"
        );
        // The instructions tell the decider a STRANDED ask is work to resume.
        assert!(
            out.contains("re-dispatch a FRESH worker"),
            "instructions must tell the decider to re-dispatch stranded asks"
        );
    }

    #[test]
    fn never_acted_goal_outranks_an_acted_one_for_fairness() {
        let p = fixture(); // has goals/triage.md
        fs::write(p.goals_dir().join("ship.md"), b"ship it\n").unwrap();
        fs::write(
            p.goal_activity(),
            format!(r#"{{"triage":{}}}"#, util::now_unix()),
        )
        .unwrap();
        assert_eq!(most_neglected_goal(&p), Some("ship".into()));

        let out = build_prompt(&p, &p.snapshots_dir());
        assert!(out.contains("=== FAIRNESS (computed by looop) ==="));
        assert!(out.contains("Most neglected goal: `ship`"));
    }

    #[test]
    fn fairness_picks_the_oldest_acted_goal_when_all_acted() {
        let p = fixture();
        fs::write(p.goals_dir().join("ship.md"), b"ship it\n").unwrap();
        let now = util::now_unix();
        fs::write(
            p.goal_activity(),
            format!(r#"{{"triage":{},"ship":{now}}}"#, now - 9999),
        )
        .unwrap();
        assert_eq!(most_neglected_goal(&p), Some("triage".into()));
    }

    #[test]
    fn no_goals_means_no_fairness_section() {
        let p = Paths::temp();
        fs::create_dir_all(p.goals_dir()).unwrap();
        fs::create_dir_all(p.asks_dir()).unwrap();
        fs::write(p.playbook(), b"pb\n").unwrap();
        assert_eq!(most_neglected_goal(&p), None);
        let out = build_prompt(&p, &p.snapshots_dir());
        assert!(!out.contains("=== FAIRNESS"));
    }
}
