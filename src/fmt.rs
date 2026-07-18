//! Progressive output formatter for the tick runner's NDJSON stream.
//!
//! `runner::run_streamed` reads the runner's raw NDJSON stdout line-by-line and
//! renders each line via `format_line` into the friendly `tool:` progress that
//! is archived to runs/<id>/output.log. There is no external formatter and looop
//! never re-execs itself to post-process its own child.

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
        "tool_execution_end" if e.get("isError").and_then(|b| b.as_bool()).unwrap_or(false) => {
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
            let dur = e.get("duration_ms").and_then(|v| v.as_u64());
            let cost = e.get("total_cost_usd").and_then(|v| v.as_f64());
            let mut parts: Vec<String> = Vec::new();
            if let Some(ms) = dur {
                parts.push(format!("{:.1}s", ms as f64 / 1000.0));
            }
            if let Some(c) = cost {
                parts.push(format!("${c:.4}"));
            }
            if parts.is_empty() {
                None
            } else {
                Some(format!("  {}done ({}){}", dim(), parts.join(" · "), rst()))
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
        .or_else(|| args.map(|a| a.to_string()))
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
