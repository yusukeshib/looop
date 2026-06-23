# looop

A tiny, portable control plane for agent-driven work. One self-contained binary —
no database, no server.

`looop` watches the things you care about (GitHub, Linear, Grafana, …) and runs a
fleet of worker agents. Every beat it senses the world and, if something changed,
makes the single most important move — including spawning workers for hands-on
work. The judgment lives **inside** looop (a small, gated LLM call per beat).

You don't drive it; you steer it. You shape *what* it pursues by editing goals and
the PLAYBOOK, and answer questions only a human can decide. Irreversible actions
(merges, deploys, deletes) always wait for your explicit yes.

![looop running a tick](demo.png)

## How it works

Each beat the pulse runs three steps:

1. **SENSE** — run every `sensors/*.sh`, refreshing `snapshots/`. If the world is
   unchanged, stop here — no LLM call, nearly free.
2. **DECIDE** — when something changed, hand the PLAYBOOK + goals + readings +
   pending asks to the LLM, which returns **one** typed move.
3. **ACT** — execute that move: write a goal/sensor/PLAYBOOK, run one reversible
   shell command, or spawn a worker. One move per beat; a daily budget caps spend.

State lives entirely in files, so the loop is **level-triggered**: it re-senses
from scratch every beat, and a crashed pulse just re-reads its files on restart.

When a worker needs a human decision, it runs a blocking `looop _ ask` and waits;
you reply with `looop _ answer`. This durable **mailbox** works for headless
workers — no tmux, no stdin.

## Concepts

Everything is plain files in the data dir (the loop's memory):

| File / dir         | Role                                                       |
| ------------------ | ---------------------------------------------------------- |
| `PLAYBOOK.md`      | your judgment, priorities, guardrails                      |
| `goals/*.md`       | desired state — one declarative spec per thing you push    |
| `sensors/*.sh`     | observers — each prints **one JSON object**                |
| `journal.md`       | action log — one line per move                             |
| `claims/`          | leases — a worker writes one to own a task                 |
| `reports/`         | deliverables a human reads                                 |
| `asks/` `answers/` | the worker ↔ human mailbox                                 |

**Workers** are the hands: detached agent sessions that do multi-step work in
parallel. Workers that touch code provision their own sandbox; looop knows nothing
about repos.

**The contract is the boundary.** You steer through the `looop _ …` verbs — they
are validated, journaled, and atomic. The file layout is the current backend,
reached *through* the contract. Editing a goal/PLAYBOOK file by hand still works
(seen next beat) but skips the journal — an escape hatch, not the main surface.

## Quick start

```sh
looop up            # start the autonomous pulse (detached)
looop watch         # (optional) live log + running-session selector
looop down          # stop the pulse and all workers
```

On first run, looop seeds a starter PLAYBOOK and a `setup` goal that invites you to
configure it. Replace it with your real work (edit goals/PLAYBOOK, or use the STEER
verbs below), and looop runs from there.

Optionally drive looop in plain language from an agent session:

```sh
pi   # then: "be my looop client — show me `looop _ state`, relay pending
     #         asks, help me edit goals; read `looop --help`"
```

## Commands

```sh
# HUMAN — looop runs itself; this is nearly all you touch
looop up [--json]              start the autonomous pulse (detached)
looop down                     stop the pulse and all workers
looop watch [<id>]             live log + session selector
looop cost [today|all|--json]  report LLM spend
looop config zsh|bash          shell integration (tab completion)
looop version | help           (help = full design manual)

# STEER — the contract; driven by you or any client
looop _ state [--json]                     read current world state
looop _ wait [--json] [--only-asks|--actionable]   block until change
looop _ asks [--json]                      pending asks only
looop _ answer <ask_id> "<text>"|- [--force]        resolve an ask
looop _ goal write <id> [body|stdin] | _ goal archive <id>
looop _ sensor write <name> [script|stdin] | _ playbook write [body|stdin]

# WORKER self-callbacks — auto-injected, not human commands
looop _ ask <id> --prompt "…" [--ref P] [--options a,b]
looop _ kill <id> | _ claim <name> | _ unclaim <name> | _ cost <…>
```

## Sensors

A sensor is a script in `sensors/` that prints **one JSON object** each beat —
looop's window onto the world. looop knows nothing about GitHub, Linear, etc.; you
teach it by dropping a sensor. Output is stored in `snapshots/<name>.json`.

Split the JSON into two keys:

- **`signal`** — the part that should WAKE the loop. A change here triggers a
  re-decide. Keep it small: counts, states, ids — not timestamps.
- **`detail`** — context that reaches the prompt but NEVER wakes the loop.

Without a `signal` key, the whole object is treated as the wake signal.

### Example: GitHub PR-review sensor

```sh
# sensors/gh.sh  (requires an authenticated `gh`)
#!/usr/bin/env bash
set -euo pipefail
cr=$(gh pr list --search "review:changes-requested" --json number --jq 'length')
open=$(gh pr list --state open --json number --jq 'length')
jq -cn --argjson cr "$cr" --argjson open "$open" \
  '{signal: {open_prs: $open, changes_requested: $cr},
    detail: {checked_at: (now | todate)}}'
```

Install with `looop _ sensor write gh < sensors/gh.sh` (or drop the file in the
data dir's `sensors/`). Its reading then appears in `looop _ state`, and a change
to `changes_requested` wakes the loop so the PLAYBOOK can react.

## Install

### curl (recommended)

Prebuilt binary from GitHub Releases — no Rust toolchain needed:

```sh
curl -fsSL https://raw.githubusercontent.com/yusukeshib/looop/main/install.sh | bash
export PATH="$HOME/.local/bin:$PATH"
```

Installs to `~/.local/bin/looop` (override with `LOOOP_INSTALL_DIR`); falls back to
`cargo install` / `nix profile install` if no prebuilt binary matches.

### Other

```sh
cargo install looop                              # crates.io
nix run github:yusukeshib/looop                  # run without installing
nix profile install github:yusukeshib/looop      # install into profile
cargo install --git https://github.com/yusukeshib/looop.git --locked looop  # latest main
```

Verify with `looop version`.

**Runtime deps:** an LLM runner (`pi` or `claude`) for the per-beat decide and
workers. Everything else (spawning, listing, killing sessions) runs in-process.
Workers that touch code also need `git` or `box` to sandbox themselves.

### Shell integration

```sh
eval "$(looop config zsh)"    # ~/.zshrc
eval "$(looop config bash)"   # ~/.bashrc
```

## Config & data

- **Config** — `$LOOOP_DATA_DIR/config.json` (override `LOOOP_CONFIG`). Runner
  wiring (`tick` command for decide, `interactive` command for workers), the pulse
  `interval`, and an optional `max_daily_usd` budget. Default runner is `pi`;
  `claude` is built in.
- **Data** — `$XDG_STATE_HOME/looop/` (override `LOOOP_DATA_DIR`). Holds the
  PLAYBOOK, goals, journal, sensors, and sessions. Not versioned for you — `git
  init` the data dir yourself if you want history. Pointing `LOOOP_DATA_DIR`
  elsewhere gives an isolated **profile**.

LLM spend is recorded in an append-only ledger (`looop cost`). Set `max_daily_usd`
to arm a daily budget breaker that skips the AI once today's spend hits the cap
(clears at local midnight).
