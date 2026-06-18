//! Surface everything that needs the human, ON the loop pane. No OS
//! notifications: we live in tmux, so a flagged worker pops a dedicated tmux
//! window. attention.md is reserved for genuine blockers; worker flags are
//! shown inline.

use crate::babysit;
use crate::events;
use crate::paths::Paths;
use crate::util;
use std::fs;
use std::process::{Command, Stdio};

pub fn surface_attention(paths: &Paths) {
    let lhd = paths.looop_hint_env();

    let flags: Vec<String> = babysit::list_looop()
        .into_iter()
        .filter(|s| s.flagged())
        .map(|s| {
            let note = s.note.clone().unwrap_or_default();
            let short = s.id.strip_prefix("looop-").unwrap_or(&s.id);
            format!(
                "  ⚑ {id}\n     {note}\n     → {lhd}looop attach {short}",
                id = s.id
            )
        })
        .collect();

    let att = fs::read_to_string(paths.data_dir.join("attention.md"))
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.lines()
                .map(|l| format!("  {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        });

    if flags.is_empty() && att.is_none() {
        return;
    }

    util::log(&format!(
        "{}{}👁  NEEDS YOU{}",
        util::yel(),
        util::b(),
        util::rst()
    ));
    if let Some(att) = &att {
        println!("{att}");
    }
    for f in &flags {
        println!("{f}");
    }

    events::emit(
        paths,
        "needs_you",
        serde_json::json!({
            "flags": flags.len(),
            "attention": att.is_some(),
        }),
    );

    tmux_surface(paths);
}

/// Pop a tmux window per newly-flagged worker. Idempotent (tracked in
/// .tmux-surfaced); unflag→reflag pops a fresh one. Disable with
/// LOOOP_TMUX_SURFACE=0.
fn tmux_surface(paths: &Paths) {
    if std::env::var("LOOOP_TMUX_SURFACE").as_deref() == Ok("0") {
        return;
    }
    if !util::on_path("tmux") {
        return;
    }
    if !tmux_ok(&["info"]) {
        return; // no server running
    }

    let flagged_ids: Vec<String> = babysit::list_looop()
        .into_iter()
        .filter(|s| s.flagged())
        .map(|s| s.id)
        .collect();

    let seen_path = paths.data_dir.join(".tmux-surfaced");
    let mut seen: Vec<String> = fs::read_to_string(&seen_path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    // Prune the seen-list to only still-flagged ids (so a re-flag pops again).
    seen.retain(|id| flagged_ids.contains(id));

    if flagged_ids.is_empty() {
        let _ = fs::write(&seen_path, seen.join("\n"));
        return;
    }

    let existing = tmux_capture(&["list-windows", "-a", "-F", "#{window_name}"]);
    let existing: Vec<&str> = existing.lines().collect();

    for id in &flagged_ids {
        if seen.contains(id) {
            continue;
        }
        let short = id.strip_prefix("looop-").unwrap_or(id);
        let wname = format!("⚑{short}");
        if existing.iter().any(|w| *w == wname) {
            continue;
        }
        // Spawn `looop attach` by absolute path (a fresh tmux shell may not have
        // looop on PATH), profile-scoped via LOOOP_DATA_DIR when non-default.
        let attach = format!(
            "{lhd}{bin} attach '{short}'",
            lhd = paths.looop_hint_env(),
            bin = paths.bin.display()
        );
        if tmux_ok(&["new-window", "-n", &wname, &attach]) {
            seen.push(id.clone());
        }
    }
    let mut body = seen.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    let _ = fs::write(&seen_path, body);
}

fn tmux_ok(args: &[&str]) -> bool {
    Command::new("tmux")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tmux_capture(args: &[&str]) -> String {
    Command::new("tmux")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}
