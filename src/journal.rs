//! `looop journal [--tail N]` — read the decision log as a first-class command.
//!
//! The journal is the human-readable record of what the loop DID: one
//! timestamped line per move (`- YYYY-MM-DD HH:MM <why>`), appended by the
//! executor. Previously you had to `cat journal.md` or scrape `looop log pulse`
//! (which is the raw structured EVENT stream, not the moves). This surfaces the
//! moves directly, with an optional tail.

use crate::paths::Paths;
use anyhow::Result;
use std::process::ExitCode;

/// The trailing `tail` lines of `lines` (all of them when `tail` is `None`).
fn tail_slice<'a>(lines: &'a [&'a str], tail: Option<usize>) -> &'a [&'a str] {
    match tail {
        Some(n) => &lines[lines.len().saturating_sub(n)..],
        None => lines,
    }
}

pub fn cmd_journal(paths: &Paths, args: &[String]) -> Result<ExitCode> {
    let mut tail: Option<usize> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--tail" | "-n" => match it.next().and_then(|v| v.trim().parse::<usize>().ok()) {
                Some(n) => tail = Some(n),
                None => {
                    eprintln!("usage: looop journal [--tail N]");
                    return Ok(ExitCode::from(1));
                }
            },
            other => {
                eprintln!(
                    "looop journal: unknown argument '{other}' (usage: looop journal [--tail N])"
                );
                return Ok(ExitCode::from(1));
            }
        }
    }

    let path = paths.journal();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        println!("looop: no journal yet ({}).", path.display());
        return Ok(ExitCode::SUCCESS);
    }
    for l in tail_slice(&lines, tail) {
        println!("{l}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_slice_takes_trailing_lines_and_handles_overshoot() {
        let lines = vec!["a", "b", "c", "d"];
        assert_eq!(tail_slice(&lines, None), &["a", "b", "c", "d"]);
        assert_eq!(tail_slice(&lines, Some(2)), &["c", "d"]);
        assert_eq!(tail_slice(&lines, Some(0)), &[] as &[&str]);
        // Asking for more than exist returns everything (no panic).
        assert_eq!(tail_slice(&lines, Some(99)), &["a", "b", "c", "d"]);
    }
}
