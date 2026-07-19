//! Four-valued reads of the small JSON state files the beat's guards keep
//! (`.tick-backoff`, `.noop-at`, the flap/decide ledgers, `.next-wake.json`).
//!
//! Every one of these files has the same failure taxonomy, and squashing the
//! states together caused real bugs (a corrupt backoff record resetting the
//! exponential wait, an EACCES read of the ledger emptying the hourly cap):
//!
//!   * ABSENT   — proven NotFound: a fresh state, never an error.
//!   * UNREADABLE — the file EXISTS but cannot be read (EACCES/EIO/…): a
//!     fail-closed caller must never mistake this for "fresh".
//!   * CORRUPT  — read fine but does not parse (torn write, debris).
//!   * PARSED   — the typed value.
//!
//! This module deliberately does NOT warn or decide policy: each caller owns
//! its own degradation message and fallback (some warn on Unreadable only,
//! some on both, some stay silent), so the shared piece is just the read +
//! classify. Callers pair it with a small `#[derive(Deserialize)]` struct per
//! state file instead of hand-walking `serde_json::Value` (the hand-walks
//! drifted apart in what they tolerated).

use std::io;
use std::path::Path;

/// One classified read of a JSON state file. See the module doc for why the
/// four states must stay distinguishable.
pub(crate) enum StateRead<T> {
    /// Proven NotFound — a fresh state.
    Absent,
    /// Present but unreadable (EACCES/EIO/…) — NOT absence.
    Unreadable(io::Error),
    /// Present and readable, but not parseable as `T`.
    Corrupt(serde_json::Error),
    /// The typed value.
    Parsed(T),
}

/// Read + classify `path` as a `T`. Never warns — the caller owns the
/// degradation policy (see the module doc).
pub(crate) fn read_state<T: serde::de::DeserializeOwned>(path: &Path) -> StateRead<T> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return StateRead::Absent,
        Err(e) => return StateRead::Unreadable(e),
    };
    match serde_json::from_str(&raw) {
        Ok(v) => StateRead::Parsed(v),
        Err(e) => StateRead::Corrupt(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Probe {
        #[serde(default)]
        n: u64,
    }

    #[test]
    fn read_state_classifies_all_four_states() {
        let p = crate::paths::Paths::temp();
        let file = p.data_dir.join("probe.json");

        // Absent: proven NotFound.
        assert!(matches!(
            read_state::<Probe>(&file),
            StateRead::<Probe>::Absent
        ));

        // Parsed: the typed value.
        std::fs::write(&file, br#"{"n": 7}"#).unwrap();
        match read_state::<Probe>(&file) {
            StateRead::Parsed(v) => assert_eq!(v, Probe { n: 7 }),
            _ => panic!("well-formed JSON must parse"),
        }

        // Corrupt: present + readable, but not JSON.
        std::fs::write(&file, b"{not json").unwrap();
        assert!(matches!(
            read_state::<Probe>(&file),
            StateRead::<Probe>::Corrupt(_)
        ));

        // Unreadable: present but the read itself fails (a DIRECTORY at the
        // path is the portable stand-in for EACCES/EIO — read_to_string errors
        // with something that is NOT NotFound).
        let as_dir = p.data_dir.join("probe-dir.json");
        std::fs::create_dir_all(&as_dir).unwrap();
        assert!(matches!(
            read_state::<Probe>(&as_dir),
            StateRead::<Probe>::Unreadable(_)
        ));
    }
}
