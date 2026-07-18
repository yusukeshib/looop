//! CLI presentation over the fleet: the `worker list` table (one-shot and
//! `--watch` in-place repaint), plus the host-local session-I/O verbs
//! (`kill`, `screenshot`). Rendering only — lifecycle policy lives in
//! [`super::launch`], the babysit adapter in [`super::fleet`].

use crate::paths::Paths;
use anyhow::Result;
use std::process::ExitCode;

use super::fleet::{PULSE_SESSION, full_session, kill, rt};

/// The pulse is the control loop, NOT a worker: refuse worker-management verbs
/// aimed at it so a stray `looop kill pulse` can't decapitate
/// or hijack the loop. Observe it with `looop screenshot pulse`; control it
/// with `looop up`/`down`. Returns true (and prints guidance) when `session` is
/// the reserved pulse id — the caller should then bail with a non-zero code.
fn reject_pulse(session: &str, verb: &str) -> bool {
    if session == PULSE_SESSION {
        eprintln!(
            "looop {verb}: '{PULSE_SESSION}' is the control loop, not a worker — observe it with \
             `looop screenshot {PULSE_SESSION}`, control it with `looop up` / `looop down`"
        );
        true
    } else {
        false
    }
}

/// `looop worker list [--json] [--all] [--watch [--interval N]]` — the fleet
/// with its health reading (the same projection the `sys-sessions` snapshot
/// feeds the decider): id, state, health, idle (since last PTY output), uptime,
/// and how long a pending ask has been waiting. Live workers only by default;
/// `--all` includes corpses. `--watch` re-renders every `--interval` seconds
/// (default 2) until Ctrl-C — the humble replacement for a fleet TUI.
pub fn cmd_worker_list(
    paths: &Paths,
    json: bool,
    all: bool,
    watch: bool,
    interval: u64,
) -> Result<ExitCode> {
    let mut prev_lines = 0usize;
    loop {
        let fleet: Vec<crate::sensor::WorkerHealth> = crate::sensor::fleet_health(paths)
            .into_iter()
            .filter(|w| all || w.alive)
            .collect();
        if json {
            let rows: Vec<serde_json::Value> = fleet
                .iter()
                .map(|w| {
                    serde_json::json!({
                        "id": w.id,
                        "state": w.state,
                        "alive": w.alive,
                        "exit_code": w.exit_code,
                        "health": w.health,
                        "idle_s": w.idle_s,
                        "uptime_s": w.uptime_s,
                        "ask_age_s": w.ask_age_s,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(ExitCode::SUCCESS);
        }
        let mut lines = 0usize;
        // Clip every line to the terminal width in watch mode: the in-place
        // repaint below counts LOGICAL lines, so a row that wraps would throw
        // off the cursor-up arithmetic and leave stale header lines behind.
        // Re-read the width each tick so a resize self-heals.
        let clip = if watch {
            crate::util::term_cols()
        } else {
            None
        };
        if watch {
            // Repaint IN PLACE, not full-screen: move the cursor up over the
            // block we drew last tick and clear from there down (\x1b[J), so
            // everything above it (prompt, prior output, scrollback) stays
            // intact. No alternate screen buffer, no \x1b[2J whole-screen wipe.
            if prev_lines > 0 {
                print!("\x1b[{prev_lines}A\x1b[J");
            }
            print_clipped(
                &format!(
                    "fleet · {}  (refresh {interval}s — Ctrl-C to stop)",
                    crate::util::date_fmt("%H:%M:%S")
                ),
                clip,
            );
            println!();
            lines += 2; // the header line + its trailing blank line
        }
        lines += render_fleet(&fleet, clip);
        if !watch {
            return Ok(ExitCode::SUCCESS);
        }
        prev_lines = lines;
        std::thread::sleep(std::time::Duration::from_secs(interval.max(1)));
    }
}

/// Print one line, clipped to `clip` columns when set (ANSI-aware). Watch
/// mode clips so no row ever wraps; one-shot mode passes `None` and prints
/// full rows.
fn print_clipped(line: &str, clip: Option<usize>) {
    match clip {
        Some(w) => println!("{}", crate::util::clip_ansi(line, w)),
        None => println!("{line}"),
    }
}

/// The plain fleet table. Columns are fixed-name, width sized to content.
/// Render the fleet table and return the number of terminal lines printed (used
/// by `--watch` to repaint in place instead of clearing the whole screen).
fn render_fleet(fleet: &[crate::sensor::WorkerHealth], clip: Option<usize>) -> usize {
    use crate::util::{dim, red, rst, yel};
    if fleet.is_empty() {
        println!("no workers");
        return 1;
    }
    let idw = fleet.iter().map(|w| w.id.len()).max().unwrap_or(2).max(2);
    print_clipped(
        &format!(
            "{}{:idw$}  {:11}  {:8}  {:>6}  {:>6}  {:>7}  VERIFY{}",
            dim(),
            "ID",
            "HEALTH",
            "STATE",
            "IDLE",
            "UP",
            "ASK",
            rst()
        ),
        clip,
    );
    for w in fleet {
        let (hl, hr) = match w.health {
            "stuck" => (red(), rst()),
            "waiting-ask" => (yel(), rst()),
            "dead" => (dim(), rst()),
            _ => ("", ""),
        };
        let state = match (w.state.as_str(), w.exit_code) {
            ("exited", Some(c)) => format!("exit {c}"),
            (s, _) => s.to_string(),
        };
        let verify = match w.verify {
            Some(true) => "pass".to_string(),
            Some(false) => format!("{}FAIL{}", red(), rst()),
            None => "-".to_string(),
        };
        print_clipped(
            &format!(
                "{:idw$}  {hl}{:11}{hr}  {:8}  {:>6}  {:>6}  {:>7}  {verify}",
                w.id,
                w.health,
                state,
                fmt_dur(w.idle_s),
                fmt_dur(w.uptime_s),
                fmt_dur(w.ask_age_s),
            ),
            clip,
        );
    }
    // 1 header row + one row per worker.
    1 + fleet.len()
}

/// Compact duration: `-` (unknown), `42s`, `5m`, `2h`, `3d`.
fn fmt_dur(secs: Option<u64>) -> String {
    match secs {
        None => "-".to_string(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86400),
    }
}

/// `looop kill <id>` — terminate a worker session (in-process). Internal
/// worker self-control callback (CONTRACT), not a human-facing verb.
pub fn cmd_kill(paths: &Paths, id: &str) -> Result<ExitCode> {
    let session = full_session(paths, id);
    if reject_pulse(&session, "kill") {
        return Ok(ExitCode::from(1));
    }
    kill(paths, &session)?;
    // The worker is dead by fiat: any tell it never drained is now addressed
    // to a corpse and must not linger for a future worker reusing the id.
    crate::mailbox::discard_tells(paths, &session);
    Ok(ExitCode::SUCCESS)
}

/// `looop screenshot <id> [--ansi|--json] [--no-trim]` — capture a session's
/// current screen (the rendered terminal grid, not a frame-by-frame append).
/// A read-only STEER verb usable on any session, including the pulse: it's how
/// a human (or any client) peeks at what a worker is showing right now without
/// attaching. Falls back to the on-disk log render if the session isn't live.
/// Defaults to plain text (cheapest for an LLM to read) with trailing blank
/// rows trimmed.
pub fn cmd_screenshot(paths: &Paths, args: &crate::cli::ScreenshotArgs) -> Result<ExitCode> {
    use ::babysit::cli::ShotFormat;
    let format = if args.ansi {
        ShotFormat::Ansi
    } else if args.json {
        ShotFormat::Json
    } else {
        ShotFormat::Plain
    };
    let trim = !args.no_trim;
    let Some(id) = args.id.as_deref() else {
        eprintln!("usage: looop screenshot <id> [--ansi|--json] [--no-trim]");
        return Ok(ExitCode::from(1));
    };
    let session = full_session(paths, id);
    rt().block_on(paths.sessions().screenshot(Some(session), format, trim))?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_dur_is_compact_and_handles_unknown() {
        assert_eq!(fmt_dur(None), "-");
        assert_eq!(fmt_dur(Some(42)), "42s");
        assert_eq!(fmt_dur(Some(300)), "5m");
        assert_eq!(fmt_dur(Some(7200)), "2h");
        assert_eq!(fmt_dur(Some(259_200)), "3d");
    }
}
