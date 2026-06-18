//! DECIDE — assemble the one-tick prompt: the PLAYBOOK, goals, sensor readings,
//! worker sessions, claims and recent journal. A faithful port of the bash
//! `build_prompt`. The instruction text is verbatim; only the marked dynamic
//! fields (data dir, binary path, local-time strings) are substituted.

use crate::babysit;
use crate::paths::Paths;
use crate::util;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const MANUAL_RUN: &str = r#"=== MANUAL RUN (human override) ===
The human explicitly asked you to act on the goal "__FOCUS__" RIGHT NOW. IGNORE the
normal priority order AND the "do nothing" option: make the single most important
move FOR THAT GOAL this run (often: start its worker session, or a small direct
action / file edit). The PLAYBOOK, the OTHER goals, sensor readings and sessions
below are CONTEXT to inform that move — act on "__FOCUS__", not on them. Still obey
every rule & guardrail, and still append exactly ONE line to journal.md.

"#;

const INSTRUCTIONS: &str = r#"You are "looop", a personal operations agent. This is one tick of a loop; your
process is disposable. Your working directory is the loop's DATA dir
(__DATA__): goals/, journal.md and sensors/ are here; edit with relative paths.

Read the PLAYBOOK, goals, sensor readings and sessions below, then make exactly
ONE move — the single most important one — and stop.

Moves:
- do a small reversible action directly (gh commands, drafts, queries)
- create / update / archive a goal (files in goals/; archive = move to goals/archive/)
- write or adjust an sensor script in sensors/ when you need a new view of the
  world; it runs from next tick. CONTRACT: print ONE small, NORMALIZED JSON
  object to stdout (capped ~8KB — it is cat'd into this prompt every beat, so a
  raw dump inflates context + cost). To avoid waking the loop on noise, split
  volatile fields out: {"signal":{… only the state that should trigger a move…},
  "detail":{… counts/timestamps/extra context…}} — only .signal feeds the
  change-detection hash, while the whole object still reaches this prompt.
- start a worker session for hands-on work (runs an agent under babysit, in the
  data dir):
    __BIN__ start-session <id> "<detailed prompt for the worker>"
  <id> matches the goal file name. The worker starts in the data dir; if its
  task edits CODE it must provision its OWN sandbox first (box if available, else
  git worktree) and cd in — say so in the prompt. Never edit code in the data dir.
- propose a PLAYBOOK change: write the FULL new PLAYBOOK to PLAYBOOK.proposed.md
  (NOT PLAYBOOK.md). The PLAYBOOK is the guardrail, so a change does NOT take
  effect until the HUMAN approves it (looop playbook approve); the loop keeps
  running on the current PLAYBOOK until then. NOTE: any edit you make to
  PLAYBOOK.md directly is automatically rolled back and parked as a proposal
  anyway — so just write PLAYBOOK.proposed.md. If a proposal is already pending
  (flagged below), do NOT add another.
- do nothing (a valid move when nothing needs doing)

Optional — set WHEN the next beat runs: you may write a single integer (seconds)
to .next-interval (one-shot, clamped 5..3600). Use your judgment per the PLAYBOOK:
tighten to keep working when a backlog is piling up, or widen when it's been
quiet a long while (spare cost / external APIs). Write nothing to keep the
default rhythm. This does NOT count as your one move.

After your move, append exactly ONE line to journal.md. Copy the timestamp
prefix below VERBATIM — it is already in this host's local time (__TZ__).
Do NOT recompute it, do NOT convert to UTC, do NOT use your own clock:
  - __DATE_HM__ <what you did and why>
(For reference, the current local time right now is __NOW__.)

"#;

const PROPOSAL_PENDING: &str = "\n=== ⚠ A PLAYBOOK CHANGE IS ALREADY PENDING HUMAN APPROVAL ===\nA previous tick proposed an edit; it is awaiting the human and is NOT yet active\n(the PLAYBOOK above is still the one in force). Do NOT propose another PLAYBOOK\nchange this tick — pick a different move or do nothing.\n";

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

pub fn build_prompt(paths: &Paths, focus: Option<&str>, snap_dir: &Path) -> String {
    let mut out = String::new();

    if let Some(f) = focus {
        out.push_str(&MANUAL_RUN.replace("__FOCUS__", f));
    }

    let instr = INSTRUCTIONS
        .replace("__DATA__", &paths.data_dir.to_string_lossy())
        .replace("__BIN__", &paths.bin.to_string_lossy())
        .replace("__TZ__", &util::date_fmt("%Z"))
        .replace("__DATE_HM__", &util::date_fmt("%Y-%m-%d %H:%M"))
        .replace("__NOW__", &util::date_fmt("%Y-%m-%d %H:%M %Z"));
    out.push_str(&instr);

    // PLAYBOOK (+ pending-proposal warning).
    out.push_str("=== PLAYBOOK ===\n");
    out.push_str(&fs::read_to_string(paths.playbook()).unwrap_or_default());
    if paths.playbook_proposed().is_file() {
        out.push_str(PROPOSAL_PENDING);
    }
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
    let sessions = babysit::list_looop();
    if sessions.is_empty() {
        out.push_str("(none)\n");
    } else {
        for s in &sessions {
            let exit = s
                .exit_code
                .map(|c| format!(" exit {c}"))
                .unwrap_or_default();
            let note = match &s.note {
                Some(n) => format!("  ⚑ {n}"),
                None => String::new(),
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
        let out = build_prompt(&p, None, &p.snapshots_dir());
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

    #[test]
    fn manual_run_focus_only_when_requested() {
        let p = fixture();
        let plain = build_prompt(&p, None, &p.snapshots_dir());
        assert!(!plain.contains("MANUAL RUN"));
        let focused = build_prompt(&p, Some("triage"), &p.snapshots_dir());
        assert!(focused.contains("MANUAL RUN"));
        assert!(focused.contains("triage"), "focus goal interpolated");
    }

    #[test]
    fn pending_proposal_warning_appears_only_when_parked() {
        let p = fixture();
        let before = build_prompt(&p, None, &p.snapshots_dir());
        assert!(!before.contains("PLAYBOOK CHANGE IS ALREADY PENDING"));
        fs::write(p.playbook_proposed(), b"a proposal\n").unwrap();
        let after = build_prompt(&p, None, &p.snapshots_dir());
        assert!(after.contains("PLAYBOOK CHANGE IS ALREADY PENDING"));
    }
}
