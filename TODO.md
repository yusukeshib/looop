# TODO — design/structure fixes (non-security)

Tracked from a critical design review. Security items (run_shell gating,
guardrail self-modification, permission bypass) are deliberately **out of scope**
here and tracked separately.

Each box is meant to land as its own commit.

## Quick wins (docs / config honesty)

- [ ] **Stale version reference.** `README.md` still says `looop version # -> looop 0.1.0`
  while `Cargo.toml` is at 0.13.0. Don't hard-code a version that drifts.
- [ ] **Model-allocation comment overgeneralizes.** `config.rs` M4 note claims "the
  default tick uses the same strong model as the worker (claude-opus-4-8)". That
  is only literally true for the `pi` runner; the `claude` runner pins no model on
  either `tick` or `interactive` (both inherit the CLI default — equal, which still
  satisfies the principle). Make the comment say what the code actually does.

## Observability

- [ ] **Sensor failures are invisible to the decider.** A user `sensors/*.sh` that
  exits non-zero (or times out, rc 124) leaves whatever partial/empty stdout it
  wrote as the snapshot; stderr goes to a `.err` file that the prompt never reads.
  The decider then reasons over a blank/garbage world and may `noop`. Replace a
  failed sensor's snapshot with a normalized error object
  (`{"error":…,"exit_code":…,"stderr":<tail>}`) so the failure (a) reaches the
  prompt and (b) participates in the world hash — which also means *fixing* a
  broken sensor wakes the loop next beat (addresses the M3 "broken sensor never
  wakes" sharp edge).

## Architecture / behavior

- [ ] **Time-driven goals are impossible; a `noop` silences the loop.** A successful
  decision (incl. `noop`) commits the world hash, so the loop won't call the AI
  again until the world changes externally. The only timer is `today.sh` (daily).
  A goal like "re-check in 5 min" cannot fire: `next_interval_s` only changes the
  sleep, then the unchanged-hash gate skips the AI anyway. Fix: when the decider
  sets `next_interval_s`, treat the next beat as a **forced re-decide** that
  bypasses the unchanged-hash skip exactly once. The AI opts in, so the
  level-triggered default is preserved.

## Deferred — needs design, not a quick patch

- [ ] **No fairness across goals (starvation).** One move per tick + "most important
  move" means a perpetually-changing high-priority goal starves the rest. K8s
  reconciles every object; looop reconciles one. Needs an aging/round-robin notion
  before it's safe to "fix" — left as a documented limitation for now.
- [ ] **Budget breaker fails open for non-pi/claude runners.** Cost metering is
  hard-wired to pi/claude NDJSON shapes; a custom runner produces no metered cost,
  so `max_daily_usd` silently never trips. At minimum, warn when a budget is set
  but no cost is ever recorded.
- [ ] **Claims are advisory only.** `start_worker` doesn't check `claims/`, and a
  worker writes its lease voluntarily; nothing enforces one-worker-per-goal. The
  "lease" (K8s analogy) has no real mutual exclusion.
- [ ] **`current_exe` re-exec fragility.** The detached supervisor re-execs the
  running binary; an upgrade / `nix gc` / move during a long-lived pulse can leave
  `current_exe` pointing at a stale or deleted inode.
- [ ] **Single-instance lock liveness.** mkdir-lock + PID-liveness check can wedge
  on PID reuse and is not safe on a shared/NFS data dir.
- [ ] **`content_hash` is a hand-rolled FNV-style hash** for the world-identity
  check. Collisions are astronomically unlikely, but a vetted hash would be one
  less bespoke primitive in the safety-critical path.
