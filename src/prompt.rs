//! DECIDE — assemble the one-tick prompt: the PLAYBOOK, goals, sensor readings,
//! worker sessions, claims and recent journal. A faithful port of the bash
//! `build_prompt`. The instruction text is verbatim; only the marked dynamic
//! fields (data dir, binary path, local-time strings) are substituted.

use crate::paths::Paths;
use crate::session;
use crate::util;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const INSTRUCTIONS: &str = r#"You are "looop", a personal operations agent. This is one tick of a loop; your
process is disposable. Your working directory is the loop's DATA dir (__DATA__).

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
     the flag up and `noop`.
  {"action":"send_key","id":"<worker>","keys":["Enter"]}   named keys (Enter, C-c)
  {"action":"restart_session","id":"<worker>"}            restart a wedged worker

  {"action":"write_playbook","body":"<full PLAYBOOK.md contents>"}
     Change your own judgment / guardrails. Deliberate — only harden a drift into
     a rule once it actually hurts.

Every action ALSO takes:
  "journal": "<one line: what you did and why>"  — looop appends it, timestamped.
  "next_interval_s": <int>  — OPTIONAL one-shot cadence nudge (clamped 5..3600):
     tighten when a backlog is piling up, widen when it's been quiet a long while.

The live worker sessions are listed below; prefer steering an existing worker
over spawning a SECOND one for the same goal. Current local time: __NOW__.

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
        if !fname.starts_with("sensor-") {
            continue;
        }
        let _ = writeln!(out, "--- {fname}");
        out.push_str(&fs::read_to_string(&o).unwrap_or_default());
        out.push('\n');
    }

    // WORKER SESSIONS.
    out.push_str("\n=== WORKER SESSIONS (babysit; ⚑note = the worker is waiting for you) ===\n");
    let sessions = session::list_workers(paths);
    if sessions.is_empty() {
        out.push_str("(none)\n");
    } else {
        for s in &sessions {
            let exit = s
                .exit_code
                .map(|c| format!(" exit {c}"))
                .unwrap_or_default();
            // Show the ⚑ only for a LIVE worker: a flag on an exited corpse is
            // stale, and surfacing it would make the pulse think a finished
            // worker is still waiting for a human.
            let note = match &s.note {
                Some(n) if s.alive => format!("  ⚑ {n}"),
                _ => String::new(),
            };
            let _ = writeln!(out, "- {} [{}{}]{}", s.id, s.state, exit, note);
        }
    }

    // WORKER CLAIMS (live leases — reaped before this point, so all are live).
    out.push_str("\n=== WORKER CLAIMS (live leases — a name with a claim here is OWNED by a worker; do NOT act on it yourself, the owner is reconciling it) ===\n");
    let claims = sorted_glob(&paths.claims_dir(), "json");
    if claims.is_empty() {
        out.push_str("(none)\n");
    } else {
        for c in claims {
            let name = c
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let body = fs::read_to_string(&c).unwrap_or_default().replace('\n', "");
            let _ = writeln!(out, "- {name}: {body}");
        }
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
            "=== PLAYBOOK ===",
            "=== GOALS ===",
            "=== WORKER SESSIONS",
            "=== WORKER CLAIMS",
            "=== RECENT JOURNAL ===",
        ] {
            assert!(out.contains(marker), "missing section: {marker}");
        }
        assert!(out.contains("PB RULES"), "playbook body inlined");
        assert!(out.contains("triage the inbox"), "goal body inlined");
    }
}
