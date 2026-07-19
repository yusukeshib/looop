//! Terminal rendering helpers: display-width math (East Asian Wide / emoji),
//! ANSI-aware clipping, and the pulse's live spinner / countdown lines.

use super::log::{color_on, dim, rst};

/// Terminal width (columns) of stdout, if stdout is a tty. Used by
/// `worker list --watch` to clip rows so they never wrap — wrapping breaks the
/// cursor-up-N in-place repaint arithmetic (the residue piles up as repeated
/// header lines in scrollback).
pub fn term_cols() -> Option<usize> {
    if !super::is_stdout_tty() {
        return None;
    }
    ratatui::crossterm::terminal::size()
        .ok()
        .map(|(cols, _rows)| cols as usize)
}

/// Display columns one char occupies in a terminal: 2 for East Asian
/// Wide/Fullwidth ranges (CJK, Hangul, kana, fullwidth forms, emoji), 1 for
/// everything else. `safe_segment` allows non-ASCII ids, so worker names CAN
/// be CJK — assuming 1 column per char would break the `worker list --watch`
/// clip/repaint arithmetic on those rows, and the `looop init` inline editor's
/// cursor math on wide glyphs in a command string. This is the ONE width
/// table (the init editor used to carry its own private copy, and the two
/// drifted on the emoji ranges) — a deliberately small inline table of the
/// big East Asian Width ranges, NOT a full unicode-width dependency: an
/// occasional 1-vs-2 miss on an exotic codepoint costs one slightly-short
/// row, which the repaint tolerates. Combining marks / zero-width joiners are
/// approximated as 1 column — acceptable for both consumers.
pub(crate) fn char_cols(c: char) -> usize {
    let cp = c as u32;
    let wide = matches!(cp,
        0x1100..=0x115F        // Hangul Jamo (leading consonants)
        | 0x2E80..=0x303E      // CJK radicals … CJK symbols/punctuation
        | 0x3041..=0x33FF      // kana, kanbun, enclosed CJK, compat
        | 0x3400..=0x4DBF      // CJK ext A
        | 0x4E00..=0x9FFF      // CJK unified
        | 0xA000..=0xA4CF      // Yi
        | 0xAC00..=0xD7A3      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compat ideographs
        | 0xFE30..=0xFE4F      // CJK compat forms
        | 0xFF00..=0xFF60      // fullwidth forms
        | 0xFFE0..=0xFFE6      // fullwidth signs
        | 0x1F300..=0x1FAFF    // emoji & pictographs (incl. transport 1F680–1F6FF,
                               // supplemental 1F900–1F9FF, extended-A 1FA70–1FAFF)
        | 0x20000..=0x3FFFD    // CJK ext B+
    );
    if wide { 2 } else { 1 }
}

/// Total display columns of a char sequence (see [`char_cols`]).
pub(crate) fn display_width(chars: impl Iterator<Item = char>) -> usize {
    chars.map(char_cols).sum()
}

/// Clip `s` to at most `max` visible columns, treating ANSI escape sequences
/// as zero-width (they are copied through, never split). If the cut happens
/// after any escape was emitted, a reset is appended so a clipped colored cell
/// can't bleed its color into the rest of the screen. Width-aware: CJK and
/// other East Asian Wide chars count as 2 columns (see [`char_cols`]) —
/// `safe_segment` allows non-ASCII ids, so 1-column-per-char is NOT a safe
/// assumption here.
///
/// NB: only SGR CSI sequences (`ESC [ … m`) are preserved; every other
/// escape is DROPPED (ESC + its bytes skipped, never copied). This is
/// defense-in-depth: every input is looop's own self-generated SGR coloring
/// (the `code!` constants), but stripping non-SGR escapes prevents
/// terminal-control injection if untrusted ANSI ever reaches here, and — for
/// non-CSI escapes like OSC (`ESC ] … BEL/ST`, no alphabetic final byte) —
/// prevents the copy-through loop from swallowing following text. Dropped
/// escapes still count as zero width, so the column budget is unaffected.
pub fn clip_ansi(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut width = 0usize;
    let mut saw_esc = false;
    let mut truncated = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Only SGR CSI (`ESC [ … m`) survives; all other escapes are
            // dropped, advancing past their bytes so they can't leak as
            // literal text or inject terminal-control sequences.
            match chars.next() {
                Some('[') => {
                    // CSI: scan to the final byte — per ECMA-48 the final byte
                    // of a CSI sequence is any of 0x40–0x7E (`@`…`~`), NOT just
                    // ASCII letters (e.g. delete-char `ESC [ 3 ~` ends in `~`).
                    // Stopping only at letters would scan past `~` and swallow
                    // the following visible text. Keep the sequence only if it
                    // is SGR (`m`).
                    let mut seq = String::from("\x1b[");
                    let mut final_byte = None;
                    for c2 in chars.by_ref() {
                        seq.push(c2);
                        if matches!(c2 as u32, 0x40..=0x7e) {
                            final_byte = Some(c2);
                            break;
                        }
                    }
                    if final_byte == Some('m') {
                        saw_esc = true;
                        out.push_str(&seq);
                    }
                }
                // Non-CSI escape: advance past its bytes so they can't leak.
                Some(d) => {
                    if matches!(d, ']' | 'P' | 'X' | '^' | '_') {
                        // OSC/DCS/SOS/PM/APC “string” escapes: terminated by
                        // BEL or ST (`ESC \`). String content may contain
                        // letters, so we must NOT stop at an alphabetic byte.
                        let mut prev_esc = false;
                        for c2 in chars.by_ref() {
                            if c2 == '\x07' {
                                break;
                            }
                            if prev_esc && c2 == '\\' {
                                break;
                            }
                            prev_esc = c2 == '\x1b';
                        }
                    } else if matches!(d, 'N' | 'O') {
                        // SS2/SS3: a single final byte follows the designator.
                        let _ = chars.next();
                    }
                    // else: `d` was itself the final byte of a 2-byte escape
                    // (`ESC =`, `ESC 7`, …) — nothing more to skip.
                }
                None => {}
            }
            continue;
        }
        let w = char_cols(c);
        if width + w > max {
            truncated = true;
            break;
        }
        out.push(c);
        width += w;
    }
    if truncated && saw_esc {
        out.push_str("\x1b[0m");
    }
    out
}

/// A lightweight, in-place "something is happening" indicator for the pulse's
/// PTY stdout while a long, otherwise-silent step runs. The tick runner can take
/// minutes and its chatter is teed to the replay archive (NOT echoed live, to
/// keep the pulse a clean structured-event log) — so without this the stream
/// goes quiet between `… is deciding the one move` and the outcome line.
///
/// Repaints ONE line every second via `\r` (`[HH:MM:SS] label elapsed`), then
/// erases it on drop so the next structured event prints clean. It is a
/// no-op unless color (ANSI) is enabled: JSON mode and `NO_COLOR` streams stay
/// byte-clean, and a non-PTY consumer never sees stray carriage returns.
///
/// STDOUT INTERLEAVING: the spinner repaints via raw `print!` with no lock
/// against `util::event`'s `println!`. The callers uphold the invariant that
/// NO events are printed while a spinner is live — it wraps exactly one
/// silent wait and is dropped before the outcome event (see the matching note
/// on [`super::event`]).
pub struct Spinner {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    /// Start the indicator (no-op when color is off). `label` is a short verb
    /// phrase, e.g. `"pi is deciding"`.
    pub fn start(label: &str) -> Self {
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let handle = if color_on() {
            let stop = stop.clone();
            let label = label.to_string();
            // Freeze the start timestamp so the line reads like a normal log
            // line (`[HH:MM:SS] <label> <elapsed>s`) — no spinner glyph; only
            // the elapsed counter advances.
            let ts = super::hms();
            Some(std::thread::spawn(move || {
                let t0 = std::time::Instant::now();
                // Repaint about once a second so the elapsed counter advances
                // visibly while keeping the PTY transcript small (~one short
                // line/sec). Poll `stop` in 100ms steps so drop() is responsive.
                while !stop.load(Ordering::Relaxed) {
                    let secs = t0.elapsed().as_secs();
                    // `write!`, never `print!`: print! panics on a write
                    // error, and this stdout is routinely a pipe whose reader
                    // can vanish — an EPIPE inside the repaint thread must
                    // not abort the long-lived pulse (observability must
                    // never fail a beat; see util::event / events.rs).
                    let _ = write!(
                        std::io::stdout().lock(),
                        "\r{}[{ts}] {label} {secs}s{}",
                        dim(),
                        rst()
                    );
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    for _ in 0..10 {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }))
        } else {
            None
        };
        Spinner { stop, handle }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        use std::io::Write;
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
            // Erase the spinner line (CR + clear-to-end-of-line) so the next
            // structured event prints on a clean line. `write!`, not `print!`:
            // an EPIPE on a dead pipe must not panic inside a Drop (which
            // would abort the process outright if it fired during an unwind).
            let _ = write!(std::io::stdout().lock(), "\r\x1b[2K");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }
}

// NB: the pulse's idle-wait countdown used to live here as `sleep_countdown`;
// it moved to `run::sleep_wake_aware`, which re-checks the wake deadline each
// second (a plain fixed-duration countdown could not be shortened mid-sleep).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_ansi_counts_only_visible_columns() {
        // Plain text: clipped at the column budget.
        assert_eq!(clip_ansi("hello world", 5), "hello");
        // Shorter than the budget: untouched.
        assert_eq!(clip_ansi("hi", 5), "hi");
        // Escapes are zero-width and copied through whole.
        assert_eq!(
            clip_ansi("\x1b[31mred\x1b[0m ok", 6),
            "\x1b[31mred\x1b[0m ok"
        );
        // A cut after a color start appends a reset so color can't bleed.
        assert_eq!(clip_ansi("\x1b[31mredredred", 3), "\x1b[31mred\x1b[0m");
    }

    #[test]
    fn clip_ansi_drops_non_sgr_escapes() {
        // Only SGR CSI (`ESC [ … m`) survives; other escapes are stripped so
        // untrusted ANSI can't inject terminal-control sequences, and
        // non-CSI escapes (OSC) can't swallow following text.
        // SGR color codes are preserved whole.
        assert_eq!(clip_ansi("\x1b[31mred\x1b[0m", 6), "\x1b[31mred\x1b[0m");
        // A non-SGR CSI (erase-line `ESC [ 2 K`) is dropped, not copied.
        assert_eq!(clip_ansi("\x1b[2Khello", 5), "hello");
        // A CSI with a NON-ALPHABETIC final byte (0x40–0x7E per ECMA-48, e.g.
        // delete-char `ESC [ 3 ~`) terminates at `~` and must not swallow the
        // visible text after it.
        assert_eq!(clip_ansi("\x1b[3~text", 4), "text");
        // An OSC sequence (set-title `ESC ] 0 ; t BEL`) is dropped and does
        // NOT swallow the text after it.
        assert_eq!(clip_ansi("\x1b]0;t\x07hi", 2), "hi");
        // A non-SGR escape between SGR codes doesn't bleed or reset tracking.
        assert_eq!(clip_ansi("\x1b[31m\x1b[2Kab", 2), "\x1b[31mab");
        // Truncation after an SGR still appends a reset; a dropped non-SGR
        // escape before the cut does not change that.
        assert_eq!(
            clip_ansi("\x1b[2K\x1b[31mredredred", 3),
            "\x1b[31mred\x1b[0m"
        );
    }

    #[test]
    fn clip_ansi_counts_cjk_as_two_columns() {
        // safe_segment allows non-ASCII ids, so CJK worker names reach the
        // watch table — each ideograph occupies 2 terminal columns.
        assert_eq!(clip_ansi("日本語", 6), "日本語");
        assert_eq!(clip_ansi("日本語", 4), "日本");
        // A wide char that would OVERSHOOT the budget is dropped whole, never
        // half-counted (5 columns fit 日本 = 4, not 日本語 = 6).
        assert_eq!(clip_ansi("日本語", 5), "日本");
        // Mixed-width rows: "w1-日本" = 3 + 4 columns.
        assert_eq!(clip_ansi("w1-日本", 5), "w1-日");
    }

    #[test]
    fn char_cols_distinguishes_wide_glyphs() {
        // The SHARED width table (watch-table clipping + the init editor):
        // narrow ASCII vs the East Asian Wide / emoji ranges.
        assert_eq!(char_cols('a'), 1);
        assert_eq!(char_cols('-'), 1);
        assert_eq!(char_cols('あ'), 2); // Hiragana
        assert_eq!(char_cols('漢'), 2); // CJK ideograph
        assert_eq!(char_cols('한'), 2); // Hangul syllable
        assert_eq!(char_cols('🎉'), 2); // emoji (1F389)
        assert_eq!(char_cols('Ａ'), 2); // fullwidth A
        // The ranges the two pre-unification copies DISAGREED on — the
        // superset must keep them wide.
        assert_eq!(char_cols('🚀'), 2); // transport & map (1F680–1F6FF)
        assert_eq!(char_cols('🧪'), 2); // supplemental (1F900–1F9FF)
        assert_eq!(char_cols('🪄'), 2); // extended-A (1FA70–1FAFF)
        assert_eq!(display_width("pi -p あ🎉".chars()), 6 + 4);
    }
}
