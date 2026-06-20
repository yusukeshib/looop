# TODO — design/structure fixes (non-security)

Tracked from a critical design review. Security items (run_shell gating,
guardrail self-modification, permission bypass) are deliberately **out of scope**
here and tracked separately.

Each box is meant to land as its own commit.

## Quick wins (docs / config honesty)

- [x] **Stale version reference.** `README.md` still says `looop version # -> looop 0.1.0`
  while `Cargo.toml` is at 0.13.0. Don't hard-code a version that drifts.
- [x] **Model-allocation comment overgeneralizes.** `config.rs` M4 note claims "the
  default tick uses the same strong model as the worker (claude-opus-4-8)". That
  is only literally true for the `pi` runner; the `claude` runner pins no model on
  either `tick` or `interactive` (both inherit the CLI default — equal, which still
  satisfies the principle). Make the comment say what the code actually does.

## Observability

- [x] **Sensor failures are invisible to the decider.** A user `sensors/*.sh` that
  exits non-zero (or times out, rc 124) leaves whatever partial/empty stdout it
  wrote as the snapshot; stderr goes to a `.err` file that the prompt never reads.
  The decider then reasons over a blank/garbage world and may `noop`. Replace a
  failed sensor's snapshot with a normalized error object
  (`{"error":…,"exit_code":…,"stderr":<tail>}`) so the failure (a) reaches the
  prompt and (b) participates in the world hash — which also means *fixing* a
  broken sensor wakes the loop next beat (addresses the M3 "broken sensor never
  wakes" sharp edge).

## Architecture / behavior

- [x] **Time-driven goals are impossible; a `noop` silences the loop.** A successful
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
- [x] **Budget breaker fails open for non-pi/claude runners.** Cost metering was
  hard-wired to pi/claude NDJSON shapes; a custom runner produced no metered cost,
  so `max_daily_usd` silently never tripped. *Done:* (1) a custom runner declares
  its cost shape via a per-runner `"cost":{type,pointer,mode}` spec, so any runner
  can be metered; (2) fail-closed — if a budget is set but the runner stays
  unmetered for `UNMETERED_LIMIT` consecutive runs, the breaker opens and skips
  the AI (self-heals when the runner/spec signature changes).
- [x] **Claims are advisory only.** Per-goal duplication was already prevented by
  the same-id alive guard in `cmd_start_session`; the real gap was the
  resource-level lease being a non-atomic `printf > claims/<name>.json` (a
  last-writer-wins TOCTOU race a worker could also just skip). *Done:* added
  `looop claim <name>` / `looop unclaim <name>` — an atomic (`O_EXCL`),
  liveness-aware test-and-set that exits non-zero when a LIVE session holds the
  lease and reclaims a stale one. The worker CONTRACT now uses it instead of raw
  file ops, so the lease is a real mutex. (looop still can't pre-know an
  arbitrary worker-chosen resource name, so enforcement is at the claim primitive,
  not at spawn — which is the right layer.)
- [ ] **`current_exe` re-exec fragility.** The detached supervisor re-execs the
  running binary; an upgrade / `nix gc` / move during a long-lived pulse can leave
  `current_exe` pointing at a stale or deleted inode. *Note:* the re-exec lives in
  the babysit library (`bs.run` re-execs `current_exe()`), not in looop — looop
  only receives it via `run_detached_worker`. A real fix is an upstream babysit
  change (accept an explicit supervisor path); not a looop-side quick win.
- [x] **Single-instance lock liveness.** Replaced the mkdir-lock + PID-liveness
  check with a `flock(2)` on `<data>/.lock/lock`: the kernel releases it when the
  pulse dies for any reason, so there is no stale lock to reclaim and no PID-reuse
  false positive that could wedge the next start. `looop status` reads liveness
  the same way. (NFS, where flock is unreliable, remains a caveat.)
- [ ] **`content_hash` is a hand-rolled FNV-style hash** for the world-identity
  check. Collisions are astronomically unlikely, but a vetted hash would be one
  less bespoke primitive in the safety-critical path.
