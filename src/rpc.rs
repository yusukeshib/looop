//! The in-worker RPC bridge (`looop _ rpc-bridge`).
//!
//! looop workers run under a PTY (the babysit `run --detached-id` supervisor):
//! whatever the `worker_command` expands to is launched on that PTY, its screen
//! bytes archived to `runs/<id>/output.log`, and `looop watch`/`client` replay
//! that log through a vt100 parser. That model assumes the worker PAINTS a
//! screen. An agent run in pi's RPC mode does not — it speaks strict JSONL over
//! stdin/stdout — so replaying it verbatim would show raw JSON, and `looop _
//! send` (which types bytes into the PTY) would feed pi malformed input.
//!
//! This bridge sits in the worker_command slot (where `claude`/`pi` would sit)
//! and translates BOTH directions so the rest of looop is unchanged:
//!
//!   worker_command = looop _ rpc-bridge --prompt-file {{prompt_file}} -- \
//!                        pi --mode rpc <flags>
//!
//! * OUT (child stdout → PTY): each JSONL event is rendered to readable text
//!   (streaming assistant deltas, `→ tool: args`, `✗ tool failed`, …) so the
//!   log/watch/client transcript reads like an interactive agent.
//! * IN (PTY → child stdin): the initial prompt (from `--prompt-file`) is sent
//!   as a `prompt` command; any later line typed at the bridge's stdin (what
//!   `looop _ send <id> "…"` writes) is wrapped as a `steer` while the agent is
//!   streaming, or a fresh `prompt` when it is idle.
//!
//! The bridge's lifetime is the child's: when the child's stdout closes (pi
//! exited) the bridge waits on it and exits with its code, so the worker
//! session ends exactly when the agent does.

use crate::cli::RpcBridgeArgs;
use crate::util::{cyan, dim, red, rst};
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

pub fn cmd_rpc_bridge(args: &RpcBridgeArgs) -> Result<ExitCode> {
    let mut child_argv = args.child.iter();
    let Some(program) = child_argv.next() else {
        eprintln!(
            "usage: looop _ rpc-bridge --prompt-file <path> -- <agent…>  \
             (e.g. -- pi --mode rpc)"
        );
        return Ok(ExitCode::from(1));
    };

    // The first prompt reaches the agent as a `prompt` command (RPC mode takes
    // no positional prompt), so read the file the worker-launch path wrote.
    let prompt = std::fs::read_to_string(&args.prompt_file)
        .with_context(|| format!("reading prompt file {}", args.prompt_file))?;

    // Pipe the child's stdin/stdout so we can interpose; leave stderr inherited
    // so any diagnostics still land on the PTY (and thus output.log) verbatim.
    let mut child: Child = Command::new(program)
        .args(child_argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning rpc agent {program:?}"))?;

    let mut child_stdin = child.stdin.take().context("child stdin unavailable")?;
    let child_stdout = child.stdout.take().context("child stdout unavailable")?;

    // Shared streaming state: the OUT loop flips it from lifecycle events; the
    // IN loop reads it to choose `steer` (mid-turn) vs `prompt` (idle).
    let streaming = Arc::new(AtomicBool::new(false));

    // IN thread: seed the first prompt, then forward each typed stdin line as a
    // prompt/steer. It owns child_stdin for its whole life. This thread is a
    // daemon — when the OUT loop returns (child gone) the process exits and
    // takes it down, so it may sit blocked on `read_line` without leaking.
    let stdin_streaming = Arc::clone(&streaming);
    thread::spawn(move || {
        if write_command(&mut child_stdin, "prompt", &prompt).is_err() {
            return;
        }
        let mut input = BufReader::new(std::io::stdin());
        let mut line = String::new();
        loop {
            line.clear();
            match input.read_line(&mut line) {
                Ok(0) | Err(_) => return, // EOF or error: stop forwarding.
                Ok(_) => {}
            }
            let text = line.trim_end_matches(['\n', '\r']);
            if text.trim().is_empty() {
                continue;
            }
            // Mid-turn text steers the running agent; idle text starts a turn.
            let verb = if stdin_streaming.load(Ordering::Relaxed) {
                "steer"
            } else {
                "prompt"
            };
            if write_command(&mut child_stdin, verb, text).is_err() {
                return;
            }
        }
    });

    // OUT loop (this thread): render the child's JSONL to the PTY until EOF.
    let mut out = std::io::stdout();
    let mut at_line_start = true;
    for line in BufReader::new(child_stdout).lines() {
        let Ok(line) = line else { break };
        // `lines()` strips `\n` but leaves a trailing `\r` if the child emits
        // CRLF; drop it so strict JSONL parsing doesn't choke on the carriage
        // return (which would silently drop the event).
        let line = line.strip_suffix('\r').unwrap_or(line.as_str());
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue; // Non-JSON on the JSONL channel: ignore (stderr carries logs).
        };
        render_event(&event, &streaming, &mut out, &mut at_line_start);
    }
    if !at_line_start {
        let _ = writeln!(out);
    }
    let _ = out.flush();

    let status = child.wait().context("waiting on rpc agent")?;
    Ok(exit_code_of(&status))
}

/// Map the child's exit status to an `ExitCode`. A normal exit forwards its
/// code (clamped to a byte); termination by signal carries no exit code, so we
/// report `128 + signal` (the shell convention) rather than a spurious `0`,
/// which would let the supervisor treat an abnormal kill as a clean exit.
fn exit_code_of(status: &std::process::ExitStatus) -> ExitCode {
    if let Some(code) = status.code() {
        return ExitCode::from(code.clamp(0, 255) as u8);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return ExitCode::from((128 + sig).clamp(0, 255) as u8);
        }
    }
    ExitCode::from(1)
}

/// Write one RPC command object (`{"type": <verb>, "message": <text>}`) as a
/// JSONL record to the child's stdin. `verb` is `prompt` (idle) or `steer`
/// (mid-turn) — the caller picks based on the current streaming state.
fn write_command(w: &mut impl Write, verb: &str, message: &str) -> std::io::Result<()> {
    let cmd = serde_json::json!({ "type": verb, "message": message });
    let mut buf = serde_json::to_string(&cmd).expect("rpc command json");
    buf.push('\n');
    w.write_all(buf.as_bytes())?;
    w.flush()
}

/// Render one RPC event to readable text on `out`, updating `streaming` from
/// lifecycle events and tracking whether the cursor sits mid-line (so tool
/// lines and message breaks start clean).
fn render_event(
    event: &Value,
    streaming: &AtomicBool,
    out: &mut impl Write,
    at_line_start: &mut bool,
) {
    let ty = event.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        // Lifecycle: track streaming so typed input routes to steer vs prompt.
        "agent_start" | "turn_start" => streaming.store(true, Ordering::Relaxed),
        "agent_end" => {
            streaming.store(false, Ordering::Relaxed);
            fresh_line(out, at_line_start);
        }
        "message_update" => {
            let Some(delta) = event.get("assistantMessageEvent") else {
                return;
            };
            let dty = delta.get("type").and_then(Value::as_str).unwrap_or("");
            match dty {
                // Stream the assistant's visible text verbatim as it arrives.
                "text_delta" => {
                    if let Some(s) = delta.get("delta").and_then(Value::as_str) {
                        let _ = write!(out, "{s}");
                        if let Some(last) = s.chars().last() {
                            *at_line_start = last == '\n';
                        }
                        let _ = out.flush();
                    }
                }
                // A new assistant text block: make sure it starts on its own line.
                "text_start" => fresh_line(out, at_line_start),
                _ => {} // thinking/toolcall deltas: tool lines come from tool_execution_*.
            }
        }
        // A tool starting is the useful progress signal (mirrors `fmt`).
        "tool_execution_start" => {
            fresh_line(out, at_line_start);
            let name = event
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let args = event.get("args");
            let raw = args
                .and_then(|a| a.get("command"))
                .or_else(|| args.and_then(|a| a.get("path")))
                .or_else(|| args.and_then(|a| a.get("file_path")))
                .and_then(|v| v.as_str().map(str::to_owned))
                .or_else(|| args.map(Value::to_string))
                .unwrap_or_default();
            let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            let collapsed: String = collapsed.chars().take(100).collect();
            let argpart = if collapsed.is_empty() {
                String::new()
            } else {
                format!("{}: {}{}", dim(), collapsed, rst())
            };
            let _ = writeln!(out, "  {}→ {}{}{}", cyan(), name, rst(), argpart);
            *at_line_start = true;
            let _ = out.flush();
        }
        "tool_execution_end"
            if event
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false) =>
        {
            fresh_line(out, at_line_start);
            let name = event
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let _ = writeln!(out, "  {}✗ {} failed{}", red(), name, rst());
            *at_line_start = true;
            let _ = out.flush();
        }
        _ => {}
    }
}

/// Emit a newline unless the cursor is already at the start of a line.
fn fresh_line(out: &mut impl Write, at_line_start: &mut bool) {
    if !*at_line_start {
        let _ = writeln!(out);
        *at_line_start = true;
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_command_emits_one_jsonl_record() {
        let mut buf: Vec<u8> = Vec::new();
        write_command(&mut buf, "prompt", "hello").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.ends_with('\n'), "must be newline-terminated JSONL");
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v["type"], "prompt");
        assert_eq!(v["message"], "hello");
    }

    #[test]
    fn write_command_escapes_embedded_newlines() {
        let mut buf: Vec<u8> = Vec::new();
        write_command(&mut buf, "steer", "line1\nline2 \"q\"").unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Exactly one framing newline: the embedded newline stays JSON-escaped.
        assert_eq!(s.matches('\n').count(), 1);
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v["type"], "steer");
        assert_eq!(v["message"], "line1\nline2 \"q\"");
    }

    /// Render one event from a clean line start; return (output, streaming, at_line_start).
    fn render(event: &Value) -> (String, bool, bool) {
        let streaming = AtomicBool::new(false);
        let mut out: Vec<u8> = Vec::new();
        let mut at_line_start = true;
        render_event(event, &streaming, &mut out, &mut at_line_start);
        (
            String::from_utf8(out).unwrap(),
            streaming.load(Ordering::Relaxed),
            at_line_start,
        )
    }

    #[test]
    fn text_delta_streams_verbatim_and_tracks_cursor() {
        let ev = serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": { "type": "text_delta", "delta": "hi there" }
        });
        let (out, _, at_line_start) = render(&ev);
        assert_eq!(out, "hi there");
        assert!(!at_line_start, "cursor sits mid-line after non-newline text");
    }

    #[test]
    fn text_delta_trailing_newline_marks_line_start() {
        let ev = serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": { "type": "text_delta", "delta": "done\n" }
        });
        let (out, _, at_line_start) = render(&ev);
        assert_eq!(out, "done\n");
        assert!(at_line_start);
    }

    #[test]
    fn tool_execution_start_formats_arrow_line() {
        let ev = serde_json::json!({
            "type": "tool_execution_start",
            "toolName": "bash",
            "args": { "command": "ls -la" }
        });
        // Colors are off unless init_color() runs, so the line is plain text.
        let (out, _, at_line_start) = render(&ev);
        assert_eq!(out, "  → bash: ls -la\n");
        assert!(at_line_start);
    }

    #[test]
    fn turn_start_sets_streaming() {
        let ev = serde_json::json!({ "type": "turn_start" });
        let (_, streaming, _) = render(&ev);
        assert!(streaming);
    }
}
