//! DECIDE — assemble the one-tick prompt: the PLAYBOOK, goals, sensor readings,
//! worker sessions, claims and recent journal. A faithful port of the bash
//! `build_prompt`. The instruction text is verbatim; only the marked dynamic
//! fields (data dir, binary path, local-time strings) are substituted.

use crate::paths::Paths;
use crate::util;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// The immutable minimal norms. Unlike the PLAYBOOK (which the AI may rewrite via
/// `write_playbook`), this lives in the binary and CANNOT be edited by any move.
/// It is injected ahead of the PLAYBOOK and OVERRIDES it on any conflict, so the
/// loop can't weaken its own irreversibility/run_shell guardrails by grooming the
/// PLAYBOOK. The PLAYBOOK is operational tuning UNDER this constitution.
const CONSTITUTION: &str = r#"These norms are FIXED (compiled into looop). They override the PLAYBOOK on any
conflict, and no move — including write_playbook — can remove or weaken them.

1. NEVER do irreversible things automatically: merging, deploying, deleting data,
   closing issues, publishing public comments, force-pushing, sending money. For
   any of these: PREPARE fully, then start (or steer) a worker that raises a
   ⚯lag and WAITS for a human. The human approves by attaching — never the AI.
2. run_shell is ONE ad-hoc, REVERSIBLE, NON-DESTRUCTIVE command only (a query, a
   draft, a read). Anything irreversible/destructive (rule 1) must NOT go through
   run_shell; it must go through a worker + ⚯lag. When unsure, treat it as
   irreversible.
3. SINGLE-WRITER POLICY FILES: only the pulse (this tick) writes PLAYBOOK.md,
   goals/ and sensors/, and only via the typed actions below — never by editing
   files directly.
4. ASK, DON'T GUESS: when you lack the information or authority to choose safely,
   surface it (send_notification, or a worker ⚯lag) rather than guess. Asking is
   cheaper than a wrong irreversible move.
5. write_playbook may tune priorities and add rules, but MUST keep these five
   norms intact. The PLAYBOOK refines judgment beneath them; it never overrides
   them.
"#;

const INSTRUCTIONS: &str = r#"You are "looop", a personal operations agent. This is one tick of a loop; your
process is disposable. Your working directory is the loop's DATA dir (__DATA__).

A fixed CONSTITUTION (below, compiled into looop) sets the non-negotiable norms.
It OVERRIDES the PLAYBOOK on any conflict, and no move can weaken it.

Read the PLAYBOOK, goals, sensor readings and sessions below, then decide the
SINGLE most important move — and stop.

You do NOT perform the move yourself. You EMIT it: write exactly ONE JSON object
describing your chosen move to `.decision.json` in your working directory. looop
— not you — then executes it. This is what guarantees one move per tick and lets
looop gate risky actions. So:
  • Do NOT edit goals/, sensors/, PLAYBOOK.md or journal.md directly.
  • Do NOT run side-effecting commands yourself. Read-only inspection to inform
    your decision is fine; the MOVE itself must be the JSON action below.
  • Emit exactly one object. If nothing needs doing, emit the `noop` action.

Pick exactly ONE `action` and fill its fields:

  {"action":"noop","reason":"why nothing is the right move"}

  {"action":"run_shell","cmd":"<one shell command>","reason":"..."}
     One ad-hoc, REVERSIBLE side-effecting command (a gh mutation, posting a
     draft…); looop runs it in the data dir. Never irreversible (merge / deploy /
     delete / public comment) — for those, start a worker that prepares it and
     raises a ⚑flag for the human.

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
     its OWN sandbox first (box if available, else git worktree) and cd in —
     never edit code in the data dir.

  {"action":"steer_session","id":"<worker>","input":"<text>"}
     Type into a LIVE worker's stdin to nudge it / answer what it asked of YOU.
     NEVER use this to answer something a worker ⚑flagged for the HUMAN — leave
     the flag up and `send_notification` so the human knows to attach (below).
  {"action":"send_key","id":"<worker>","keys":["Enter"]}   named keys (Enter, C-c)
  {"action":"restart_session","id":"<worker>"}            restart a wedged worker

  {"action":"send_notification","message":"<what the human must know / decide>",
   "id":"<flagged worker to attach, optional>"}
     Surface a blocker or notice to the human — journaled and shown on this
     tick's line. If the operator wired a `notification` command in config it
     ALSO fires (e.g. pops a tmux window onto the worker); pass `id` = the
     flagged worker's session id so that hook can `looop attach` it. Two cases:
       1. YOU, the pulse, are blocked on a human editing the world (a goal, the
          PLAYBOOK, creds, a priority call) — which the next tick observes.
       2. A worker is ⚑flagged and waiting for the human: relay its note and tell
          them how to answer, e.g. "fix-pr-2143 waiting: <note> → looop attach
          fix-pr-2143". The flag stays up in WORKER SESSIONS until they answer,
          so notify ONCE — don't re-notify the same flag every tick.
     There is NO reply channel and NO state kept on the notice itself: for a
     question whose answer must flow back INTO running work, the worker's own
     ⚑flag (above) is the session-backed channel — the human attaches to reply.

  {"action":"write_playbook","body":"<full PLAYBOOK.md contents>"}
     Change your own judgment / guardrails. Deliberate — only harden a drift into
     a rule once it actually hurts.

Every action ALSO takes:
  "journal": "<one line: what you did and why>"  — looop appends it, timestamped.
  "next_interval_s": <int>  — OPTIONAL one-shot cadence nudge (clamped 5..3600):
     tighten when a backlog is piling up, widen when it's been quiet a long while.
     It ALSO forces the next beat to re-decide even if nothing in the world
     changed — use it for a time-based follow-up ("re-check in N seconds"), since
     an unchanged world otherwise skips the AI entirely.

Three of the SENSOR READINGS are looop's OWN state (system sensors, not
sensors/*.sh):
  • sys-sessions — the live worker fleet. An entry with a `note` means that worker
    raised a ⚑flag and is WAITING for the human: relay it via send_notification;
    do NOT answer a human-flag yourself. Prefer steering an existing worker over
    spawning a SECOND one for the same goal.
  • sys-claims — live worker leases. A name listed here is OWNED by the worker
    reconciling it; do NOT act on it yourself.
  • sys-goals — per-goal staleness (.detail.goals[id].age_s = seconds since you
    last acted on that goal; null = never). FAIRNESS: you pick ONE move per beat,
    so a constantly-changing goal can starve the rest. When several goals are
    ready and roughly comparable, prefer the one you've neglected longest rather
    than always serving the loudest. (Workers run in parallel, so dispatching a
    neglected goal doesn't block the others.)
Current local time: __NOW__.

Write your single JSON object to `.decision.json` now, then stop.

"#;

fn sorted_glob(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == ext).unwrap_or(false))
        .collect();
    v.sort();
    v
}

/// The single most-neglected goal: the top-level `goals/*.md` looop has gone
/// longest without acting on (a goal never acted on outranks any acted one).
/// `None` when there are no goals. Computed by looop — not left to the AI to scan
/// — so the fairness nudge names a concrete goal the decider must justify
/// skipping (RULE: one move/beat can otherwise starve the quiet goals).
fn most_neglected_goal(paths: &Paths) -> Option<String> {
    let activity: serde_json::Map<String, serde_json::Value> =
        fs::read_to_string(paths.goal_activity())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
    let mut goals: Vec<String> = fs::read_dir(paths.goals_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .collect();
    goals.sort(); // deterministic tie-break
    // last-acted unix; never-acted => 0 (oldest possible) => ranked most neglected.
    goals
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

    let instr = INSTRUCTIONS
        .replace("__DATA__", &paths.data_dir.to_string_lossy())
        .replace("__NOW__", &util::date_fmt("%Y-%m-%d %H:%M %Z"));
    out.push_str(&instr);

    // CONSTITUTION (immutable, binary-embedded) — ahead of the PLAYBOOK and
    // overriding it. The AI can rewrite the PLAYBOOK but never this.
    out.push_str("=== CONSTITUTION (immutable — overrides PLAYBOOK) ===\n");
    out.push_str(CONSTITUTION);
    out.push('\n');

    // PLAYBOOK.
    out.push_str("=== PLAYBOOK ===\n");
    out.push_str(&fs::read_to_string(paths.playbook()).unwrap_or_default());
    out.push('\n');

    // GOALS.
    out.push_str("\n=== GOALS ===\n");
    let goals = sorted_glob(&paths.goals_dir(), "md");
    if goals.is_empty() {
        out.push_str("(no goals yet)\n");
    } else {
        for g in goals {
            let name = g.file_name().unwrap_or_default().to_string_lossy();
            let _ = writeln!(out, "--- {name}");
            out.push_str(&fs::read_to_string(&g).unwrap_or_default());
            out.push('\n');
        }
    }

    // SENSOR READINGS.
    out.push_str("\n=== SENSOR READINGS ===\n");
    for o in sorted_glob(snap_dir, "json") {
        let fname = o
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        // User sensors (`sensor-*`) and the virtual system sensors (`sys-*`) are
        // one uniform stream here — the fleet and leases arrive as `sys-sessions`
        // / `sys-claims`, no bespoke per-kind sections.
        if !(fname.starts_with("sensor-") || fname.starts_with("sys-")) {
            continue;
        }
        let _ = writeln!(out, "--- {fname}");
        out.push_str(&fs::read_to_string(&o).unwrap_or_default());
        out.push('\n');
    }

    // FAIRNESS (computed by looop, not left to the AI to eyeball sys-goals).
    // Naming the concrete most-neglected goal turns the advisory staleness
    // reading into a directive the decider must answer to: serve it, or justify
    // skipping it in the journal.
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
    match fs::read_to_string(paths.journal()) {
        Ok(j) if !j.is_empty() => {
            out.push_str(&tail_lines(&j, 20));
            out.push('\n');
        }
        _ => out.push_str("(empty)\n"),
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Paths {
        let p = Paths::temp();
        fs::create_dir_all(p.goals_dir()).unwrap();
        fs::create_dir_all(p.claims_dir()).unwrap();
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
            "=== RECENT JOURNAL ===",
        ] {
            assert!(out.contains(marker), "missing section: {marker}");
        }
        // The immutable norms are inlined ahead of the (mutable) PLAYBOOK.
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
    fn never_acted_goal_outranks_an_acted_one_for_fairness() {
        let p = fixture(); // has goals/triage.md
        fs::write(p.goals_dir().join("ship.md"), b"ship it\n").unwrap();
        // triage was acted on recently; ship never has => ship is most neglected.
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
        // triage acted long ago, ship acted just now => triage is most neglected.
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
        fs::write(p.playbook(), b"pb\n").unwrap();
        assert_eq!(most_neglected_goal(&p), None);
        let out = build_prompt(&p, &p.snapshots_dir());
        assert!(!out.contains("=== FAIRNESS"));
    }
}
