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
    use crate::util::{b, dim, red, rst};
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
        "tool_execution_start" => {
            let name = e.get("toolName").and_then(|t| t.as_str()).unwrap_or("tool");
            let args = e.get("args");
            let raw = args
                .and_then(|a| a.get("command"))
                .or_else(|| args.and_then(|a| a.get("path")))
                .or_else(|| args.and_then(|a| a.get("file_path")))
                .and_then(|v| v.as_str().map(str::to_owned))
                .or_else(|| args.map(|a| a.to_string()))
                .unwrap_or_default();
            let collapsed: String = collapse_ws(&raw).chars().take(100).collect();
            let argpart = if collapsed.is_empty() {
                String::new()
            } else {
                format!("{}: {}{}", dim(), collapsed, rst())
            };
            // Whole line dim: tool lines are background progress, not signal.
            // No glyph — the dim color alone marks it as background.
            Some(format!("  {}{}{}{}", dim(), name, rst(), argpart))
        }
        "tool_execution_end" if e.get("isError").and_then(|b| b.as_bool()).unwrap_or(false) => {
            let name = e.get("toolName").and_then(|t| t.as_str()).unwrap_or("tool");
            // No glyph — the failure signal rides on the text color (red +
            // bold), mirroring the pulse's Error lines.
            Some(format!("  {}{}{} failed{}", red(), b(), name, rst()))
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
        _ => None,
    }
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
}
