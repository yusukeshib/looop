//! Progressive output formatter for the tick runner's NDJSON stream.
//!
//! `runner::run_streamed` reads the runner's raw NDJSON stdout line-by-line and
//! renders each line via `format_line` into the friendly `tool:` progress that
//! is archived to runs/<id>/output.log. There is no external formatter and looop
//! never re-execs itself to post-process its own child.
//!
//! Three runner schemas are recognized: pi (`--mode json`), claude
//! (`-p --output-format stream-json`, the default tick wiring) and codex
//! (`exec --json`). Codex support is BEST-EFFORT: its event schema
//! (`thread.started` / `item.started` / `item.completed` … with a nested
//! `item` object) is still marked experimental upstream, so the arm below
//! reads only the stable-looking generic shapes (`item.type`, `text`,
//! `command`, `exit_code`) defensively — an unrecognized codex event degrades
//! to silence, never to a crash or garbage in the archive.

/// Render one NDJSON event line; `None` means "emit nothing" (mirrors jq empty).
/// Used in-process by `runner::run_streamed` to turn the tick runner's raw
/// stream into the friendly progress lines archived to runs/<id>/output.log.
pub(crate) fn format_line(line: &str) -> Option<String> {
    use crate::util::{dim, red, rst};
    let Ok(e) = serde_json::from_str::<serde_json::Value>(line) else {
        // Non-JSON: pass through unchanged, but swallow empty lines.
        return if line.is_empty() {
            None
        } else {
            Some(line.to_string())
        };
    };
    let ty = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        // ---- pi `--mode json` schema -------------------------------------
        "tool_execution_start" => {
            let name = e.get("toolName").and_then(|t| t.as_str()).unwrap_or("tool");
            Some(tool_line(name, e.get("args")))
        }
        "tool_execution_end"
            if e.get("isError")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false) =>
        {
            let name = e.get("toolName").and_then(|t| t.as_str()).unwrap_or("tool");
            // No glyph — the failure signal rides on the text color (red),
            // mirroring the pulse's Error lines.
            Some(format!("  {}{} failed{}", red(), name, rst()))
        }
        "message_end"
            if e.pointer("/message/role").and_then(|r| r.as_str()) == Some("assistant") =>
        {
            let text: String = e
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<String>()
                })
                .unwrap_or_default();
            if text.is_empty() {
                None
            } else {
                Some(format!("\n{text}"))
            }
        }
        // ---- claude `-p --output-format stream-json` schema ---------------
        // (the DEFAULT tick wiring). Events look like:
        //   {"type":"system","subtype":"init",...}
        //   {"type":"assistant","message":{"role":"assistant","content":[
        //       {"type":"text","text":"…"},
        //       {"type":"tool_use","name":"Bash","input":{"command":"…"}}]}}
        //   {"type":"user",...}          (tool results echoed back)
        //   {"type":"result","duration_ms":…,"total_cost_usd":…,...}
        "assistant" => {
            let blocks = e.pointer("/message/content").and_then(|c| c.as_array())?;
            let mut lines: Vec<String> = Vec::new();
            let mut text = String::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => {
                        let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        lines.push(tool_line(name, b.get("input")));
                    }
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            if !text.trim().is_empty() {
                // Assistant narration is background progress too — dim it.
                lines.push(format!("\n{}{}{}", dim(), text, rst()));
            }
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        }
        "result" => {
            // One tail summary line: duration/cost when claude reports them.
            let dur = e.get("duration_ms").and_then(serde_json::Value::as_u64);
            let cost = e.get("total_cost_usd").and_then(serde_json::Value::as_f64);
            let mut parts: Vec<String> = Vec::new();
            if let Some(ms) = dur {
                parts.push(format!("{:.1}s", ms as f64 / 1000.0));
            }
            if let Some(c) = cost {
                parts.push(format!("${c:.4}"));
            }
            // A FAILED tick must not render as a dim `done (…)`: claude flags
            // failure via `is_error: true` and/or an `error_*` subtype
            // (error_max_turns, error_during_execution, …). Surface it as the
            // same red no-glyph failure line the pi/codex arms use, naming the
            // subtype so the archive says WHY.
            let subtype = e.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            let failed = e
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                || subtype.starts_with("error");
            if failed {
                let mut why: Vec<String> = Vec::new();
                if !subtype.is_empty() {
                    why.push(subtype.to_string());
                }
                why.extend(parts);
                return Some(format!("  {}failed ({}){}", red(), why.join(" · "), rst()));
            }
            if parts.is_empty() {
                None
            } else {
                Some(format!("  {}done ({}){}", dim(), parts.join(" · "), rst()))
            }
        }
        // ---- codex `exec --json` schema (best-effort — see module doc) ----
        // Events wrap a nested `item`; the shapes we consume look like:
        //   {"type":"item.started","item":{"type":"command_execution",
        //       "command":"bash -lc …","status":"in_progress"}}
        //   {"type":"item.completed","item":{"type":"command_execution",
        //       "command":"…","exit_code":1,"status":"failed"}}
        //   {"type":"item.completed","item":{"type":"agent_message","text":"…"}}
        "item.started" | "item.completed" => {
            let item = e.get("item")?;
            match item.get("type").and_then(|t| t.as_str()) {
                // The command line is shown once, when execution STARTS (the
                // progress signal); completion only matters when it FAILED.
                Some("command_execution") if ty == "item.started" => {
                    let cmd = item.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    Some(tool_line(
                        "command",
                        Some(&serde_json::json!({ "command": cmd })),
                    ))
                }
                Some("command_execution") => {
                    let failed = item
                        .get("exit_code")
                        .and_then(serde_json::Value::as_i64)
                        .is_some_and(|c| c != 0);
                    // Same red no-glyph failure line as the pi arm.
                    failed.then(|| format!("  {}command failed{}", red(), rst()))
                }
                // The assistant's narration, complete text at item.completed
                // (deltas ride item.updated, which we skip — the archive wants
                // whole messages, not a character stream).
                Some("agent_message") if ty == "item.completed" => {
                    let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if text.trim().is_empty() {
                        None
                    } else {
                        Some(format!("\n{text}"))
                    }
                }
                // reasoning / file_change / mcp_tool_call / web_search /
                // unknown item kinds: no progress worth archiving (defensive —
                // the schema is experimental upstream).
                _ => None,
            }
        }
        // codex fatal stream error: surface it — otherwise the run archive
        // ends silently mid-stream with no hint why.
        "error" => {
            let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
            if msg.is_empty() {
                None
            } else {
                Some(format!("  {}error: {msg}{}", red(), rst()))
            }
        }
        // claude bookkeeping events carry no progress worth archiving.
        "system" | "user" => None,
        // Generic fallback: any other well-formed JSON event stays silent
        // (non-JSON lines already passed through above).
        _ => None,
    }
}

/// One dim `  <tool>: <args>` progress line, shared by the pi
/// `tool_execution_start` arm and claude's `tool_use` content blocks.
fn tool_line(name: &str, args: Option<&serde_json::Value>) -> String {
    use crate::util::{dim, rst};
    let raw = args
        .and_then(|a| a.get("command"))
        .or_else(|| args.and_then(|a| a.get("path")))
        .or_else(|| args.and_then(|a| a.get("file_path")))
        .and_then(|v| v.as_str().map(str::to_owned))
        .or_else(|| args.map(std::string::ToString::to_string))
        .unwrap_or_default();
    // LOG output — full command (whitespace collapsed, never truncated).
    let collapsed: String = collapse_ws(&raw);
    let argpart = if collapsed.is_empty() {
        String::new()
    } else {
        format!("{}: {}{}", dim(), collapsed, rst())
    };
    // Whole line dim: tool lines are background progress, not signal.
    // No glyph — the dim color alone marks it as background.
    format!("  {}{}{}{}", dim(), name, rst(), argpart)
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collapse_ws_squeezes_all_whitespace() {
        assert_eq!(collapse_ws("  a\t b\n c "), "a b c");
        assert_eq!(collapse_ws(""), "");
    }

    #[test]
    fn format_line_passthrough_and_empty() {
        // Non-JSON passes through; blank lines are swallowed.
        assert_eq!(format_line("plain text"), Some("plain text".to_string()));
        assert_eq!(format_line(""), None);
    }

    #[test]
    fn format_line_assistant_text_and_skips() {
        let msg = json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": [ { "type": "text", "text": "hi" } ] }
        })
        .to_string();
        assert_eq!(format_line(&msg), Some("\nhi".to_string()));

        // Empty assistant text emits nothing.
        let empty = json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": [] }
        })
        .to_string();
        assert_eq!(format_line(&empty), None);

        // Unknown event types emit nothing.
        let other = json!({ "type": "session_start" }).to_string();
        assert_eq!(format_line(&other), None);
    }

    #[test]
    fn format_line_claude_assistant_text_and_tool_use() {
        // Real-shaped claude `--output-format stream-json` assistant event.
        let ev = json!({
            "type": "assistant",
            "message": {
                "id": "msg_1",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "checking the repo" },
                    { "type": "tool_use", "id": "tu_1", "name": "Bash",
                      "input": { "command": "git   status" } }
                ]
            },
            "session_id": "s1"
        })
        .to_string();
        let out = format_line(&ev).expect("assistant event renders");
        assert!(out.contains("Bash"), "tool name surfaced: {out}");
        assert!(
            out.contains("git status"),
            "command collapsed + shown: {out}"
        );
        assert!(
            out.contains("checking the repo"),
            "text block surfaced: {out}"
        );

        // Tool-use only (no text) still yields the LOG line.
        let tool_only = json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [
                { "type": "tool_use", "name": "Read", "input": { "file_path": "/tmp/x" } }
            ]}
        })
        .to_string();
        let out = format_line(&tool_only).expect("tool_use renders");
        assert!(out.contains("Read"));
        assert!(out.contains("/tmp/x"));

        // Empty content emits nothing.
        let empty = json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [] }
        })
        .to_string();
        assert_eq!(format_line(&empty), None);
    }

    #[test]
    fn format_line_codex_items_render_progress() {
        // Representative codex `exec --json` lines (schema is experimental
        // upstream — these mirror the shapes the arm reads defensively).
        // A command execution surfaces its command line when it STARTS…
        let started = json!({
            "type": "item.started",
            "item": { "id": "item_1", "type": "command_execution",
                      "command": "bash -lc 'git   status'", "status": "in_progress" }
        })
        .to_string();
        let out = format_line(&started).expect("codex command start renders");
        assert!(
            out.contains("git status"),
            "command collapsed + shown: {out}"
        );

        // …a CLEAN completion adds nothing (the start line already logged it)…
        let ok = json!({
            "type": "item.completed",
            "item": { "type": "command_execution", "command": "ls",
                      "exit_code": 0, "status": "completed" }
        })
        .to_string();
        assert_eq!(format_line(&ok), None);

        // …but a FAILED one is surfaced (red, no glyph — like the pi arm).
        let failed = json!({
            "type": "item.completed",
            "item": { "type": "command_execution", "command": "ls",
                      "exit_code": 1, "status": "failed" }
        })
        .to_string();
        let out = format_line(&failed).expect("codex command failure renders");
        assert!(out.contains("failed"), "{out}");

        // The assistant's message lands whole at item.completed.
        let msg = json!({
            "type": "item.completed",
            "item": { "id": "item_2", "type": "agent_message", "text": "all done" }
        })
        .to_string();
        assert_eq!(format_line(&msg), Some("\nall done".to_string()));

        // Reasoning + bookkeeping events stay silent; a stream error surfaces.
        let reasoning = json!({
            "type": "item.completed",
            "item": { "type": "reasoning", "text": "thinking…" }
        })
        .to_string();
        assert_eq!(format_line(&reasoning), None);
        let turn = json!({ "type": "turn.completed", "usage": { "input_tokens": 5 } }).to_string();
        assert_eq!(format_line(&turn), None);
        let err = json!({ "type": "error", "message": "stream disconnected" }).to_string();
        let out = format_line(&err).expect("codex stream error renders");
        assert!(out.contains("stream disconnected"), "{out}");
    }

    #[test]
    fn format_line_pi_tool_execution_start_and_failed_end() {
        // Real-shaped pi `--mode json` tool events.
        let start = json!({
            "type": "tool_execution_start", "toolName": "bash",
            "args": { "command": "git   status" }
        })
        .to_string();
        let out = format_line(&start).expect("pi tool start renders");
        assert!(out.contains("bash"), "tool name surfaced: {out}");
        assert!(
            out.contains("git status"),
            "command collapsed + shown: {out}"
        );

        // No args at all still yields the bare tool line…
        let bare = json!({ "type": "tool_execution_start" }).to_string();
        let out = format_line(&bare).expect("argless tool start renders");
        assert!(out.contains("tool"), "{out}");

        // …a CLEAN end adds nothing, a FAILED one is surfaced (red, no glyph).
        let ok = json!({ "type": "tool_execution_end", "toolName": "bash", "isError": false })
            .to_string();
        assert_eq!(format_line(&ok), None);
        let failed = json!({ "type": "tool_execution_end", "toolName": "bash", "isError": true })
            .to_string();
        let out = format_line(&failed).expect("pi tool failure renders");
        assert!(out.contains("bash failed"), "{out}");
    }

    #[test]
    fn format_line_claude_result_error_renders_failure_line() {
        // is_error + error subtype: the archive must say FAILED, not `done`.
        let err = json!({
            "type": "result", "subtype": "error_max_turns", "is_error": true,
            "duration_ms": 12345, "total_cost_usd": 0.0421
        })
        .to_string();
        let out = format_line(&err).expect("error result renders");
        assert!(out.contains("failed"), "{out}");
        assert!(out.contains("error_max_turns"), "subtype surfaced: {out}");
        assert!(out.contains("12.3s"), "duration still shown: {out}");
        assert!(
            !out.contains("done"),
            "a failed tick never reads `done`: {out}"
        );

        // An error subtype alone (no is_error field) is still a failure…
        let sub = json!({ "type": "result", "subtype": "error_during_execution" }).to_string();
        let out = format_line(&sub).expect("error subtype renders");
        assert!(out.contains("error_during_execution"), "{out}");

        // …and a bare is_error with no subtype/metrics still surfaces.
        let bare = json!({ "type": "result", "is_error": true }).to_string();
        let out = format_line(&bare).expect("bare error result renders");
        assert!(out.contains("failed"), "{out}");
    }

    #[test]
    fn format_line_claude_result_system_and_user() {
        // Result: duration + cost condensed into one summary line.
        let res = json!({
            "type": "result", "subtype": "success", "is_error": false,
            "duration_ms": 12345, "num_turns": 3, "total_cost_usd": 0.0421,
            "result": "done"
        })
        .to_string();
        let out = format_line(&res).expect("result renders");
        assert!(out.contains("12.3s"), "{out}");
        assert!(out.contains("$0.0421"), "{out}");

        // Result with neither duration nor cost emits nothing.
        let bare = json!({ "type": "result", "subtype": "success" }).to_string();
        assert_eq!(format_line(&bare), None);

        // Init/system + user (tool result echo) events are skipped.
        let init = json!({ "type": "system", "subtype": "init", "model": "x" }).to_string();
        assert_eq!(format_line(&init), None);
        let user = json!({ "type": "user", "message": { "role": "user" } }).to_string();
        assert_eq!(format_line(&user), None);
    }
}
