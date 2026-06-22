# looop

A tiny, portable control plane for agent-driven work.

`looop` watches the things you care about (GitHub, Linear, Grafana, …) and runs
your worker fleet. It is **autonomous**: each beat it senses the world and, when
something changed, decides the single most important move and executes it —
spawning worker agents for hands-on work. The **judgment lives inside looop** (a
small, gated LLM call per beat); looop is the brain.

You are a peer, not the driver: you steer by editing goals / the PLAYBOOK, and a
worker that hits a decision only a human can make asks you and waits. To watch and
steer in plain language you can run a **concierge** — a pi/claude session pointed
at looop that reads its state and relays — but looop runs fine without one. One
self-contained binary, no database, no server.

![looop running a tick](demo.png)

## How it works

looop is the brain; workers are the hands; you (optionally via a concierge) are
the peer who shapes goals and answers what only a human can.

```
   pulse (looop — autonomous)              workers            concierge (optional)
   ──────────────────────────             ───────            ────────────────────
   each beat: sense the world,            real agents        a pi/claude YOU run:
   if it changed → decide ONE     ──▶     doing multi-       reads `looop _ state`,
   move → execute it (gated)              step work          relays asks to you,
   (skips the LLM if unchanged)           `ask` + wait  ──▶  helps you edit goals
```

1. **SENSE** — the pulse runs every `sensors/*.sh` each beat, keeping
   `snapshots/` fresh. If the world is unchanged since last beat it stops here —
   no LLM call, nearly free.
2. **DECIDE** — when the world changed, looop hands the PLAYBOOK + goals +
   readings + pending asks to the `tick` runner, which emits ONE typed move.
3. **ACT** — looop executes that move (and gates risky ones): write a
   goal/sensor/PLAYBOOK, run one reversible shell command, start a worker, or
   notify you. One move per beat; a daily budget breaker caps spend.
4. **HUMAN** — you steer by editing goals/PLAYBOOK (observed next beat); a worker
   that needs a human decision `ask`s and waits. Irreversible things never happen
   without your explicit yes.

State lives entirely in files (goals, snapshots, journal, mailbox), so it is
**level-triggered**: the pulse re-senses from scratch every beat and a crashed
pulse just re-reads the unanswered asks on restart.

The human-in-the-loop path is a durable **mailbox**, not a tmux prompt: a worker
that needs a decision runs one blocking `looop _ ask …` and waits; you answer with
`looop _ answer` (directly or through the concierge). No attach, no stdin
wrangling — it works for headless workers.

## Concepts

Everything lives as plain files in the data dir (the loop's memory):

| File / dir      | Role (Kubernetes analogy)                                          |
| --------------- | ------------------------------------------------------------------ |
| `PLAYBOOK.md`   | the controller logic — your judgment, priorities, guardrails       |
| `goals/*.md`    | desired state — one declarative spec per thing you're pushing      |
| `sensors/*.sh`  | observers — each prints **one JSON object** describing the world   |
| `journal.md`    | the action log — one line per move                                 |
| `claims/`       | leases — a worker writes one to *own* a task; stale ones auto-reap |
| `reports/`      | deliverables a human reads (persists across beats)                 |
| `asks/` `answers/` | the worker ↔ human mailbox (questions + answers)                |

**Workers** are the hands. When a move needs real, multi-step work, looop spawns
an agent session that runs detached, in parallel, and reconciles its task on its
own. Workers that touch code provision their own sandbox first; looop itself knows
nothing about repos.

**Humans in the loop.** looop decides on its own — you shape WHAT it pursues by
editing goals / the PLAYBOOK (it observes the change next beat). A worker that
needs a decision only a human can make runs `looop _ ask` and blocks; you answer
with `looop _ answer` — directly, or through a **concierge** (a pi/claude session
you point at looop that surfaces pending asks and helps you edit goals). The
concierge is an interface, not a decision-maker. Irreversible actions (merges,
deploys, deletes) always require your explicit approval via an ask.

## Quick start

```sh
looop up            # start the autonomous pulse (sense + decide + run workers), detached
looop watch         # (optional) live colored log + running-session selector
# (optional) run a concierge to watch + steer in plain language:
pi                  # then say: "be my looop concierge — show me `looop _ state`,
                    #            relay pending asks, help me edit goals; read `looop --help`"
looop down          # stop the pulse and all workers
```

`looop up` starts the autonomous pulse — looop runs itself from there. You steer
by editing goals/PLAYBOOK and answering asks (`looop _ answer`); the concierge is
optional sugar for doing that in chat. `looop up --json` makes the pulse log
machine-readable NDJSON.

On the first run looop seeds a starter PLAYBOOK and a `setup` goal whose only job
is to **notify you to configure it** — run a concierge (or edit goals/PLAYBOOK
directly) to replace the starter with your real work. After that it just runs.

## Commands

```sh
# HUMAN (looop runs itself — this is nearly all you touch)
looop up [--json]              start the autonomous pulse (detached)
looop down                     stop the pulse and all workers
looop watch [<id>]             observer TUI: live colored log + session selector
looop cost [today|all|--json]  report LLM spend (per-beat decide + workers)
looop config zsh|bash          print shell integration (tab completions)
looop version | help           (looop help = the full design manual)

# STEER (you, or a concierge acting for you — looop does NOT need these to act)
looop _ state [--json] | _ wait [--json]   read current world state
looop _ answer <ask_id> "<text>"           resolve a worker's pending ask
looop _ goal write <id> [body|stdin] | _ goal archive <id>
looop _ sensor write <name> [script|stdin] | _ playbook write [body|stdin]
looop _ notify <message…>                  surface a notice (journaled)

# WORKER self-callbacks (auto-injected contract — not human commands)
looop _ ask <id> --prompt "…" [--ref P] [--options a,b]   ask + block for answer
looop _ kill <id> | _ claim <name> | _ unclaim <name> | _ cost <…>
```

The human surface is tiny — essentially `up`/`down`/`watch` (plus `cost`/`config`).
looop decides autonomously; the `looop _ …` STEER verbs let you (or a concierge)
inspect state, answer asks, and edit goals/sensors/PLAYBOOK by hand, and
**workers** self-report (ask, kill, claim, unclaim, cost) via the auto-injected
contract.

## Shell integration

```sh
# Zsh (~/.zshrc)
eval "$(looop config zsh)"

# Bash (~/.bashrc)
eval "$(looop config bash)"
```

This adds tab completion for looop's (small) human command surface.

To change judgment: run `looop _ playbook write` (or edit a goal) — looop picks it
up next beat.

## Install

### curl (recommended)

Downloads a prebuilt binary from GitHub Releases — **no Rust toolchain needed**:

```sh
curl -fsSL https://raw.githubusercontent.com/yusukeshib/looop/main/install.sh | bash
```

Installs `looop` to `~/.local/bin/looop` (override with `LOOOP_INSTALL_DIR`). The
script falls back to `cargo install` / `nix profile install` if no prebuilt
binary matches your platform. Make sure the install dir is on your `PATH`:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

### Cargo

```sh
cargo install looop
```

### Nix (flakes)

```sh
nix run github:yusukeshib/looop                 # run without installing
nix profile install github:yusukeshib/looop     # install into your profile
nix develop github:yusukeshib/looop             # dev shell (cargo, clippy, rustfmt)
```

### From git (latest `main`)

```sh
cargo install --git https://github.com/yusukeshib/looop.git --locked looop
```

### Verify

```sh
looop version   # prints the installed version (e.g. looop 0.13.0)
looop help
```

Runtime deps: just an LLM runner (`pi` or `claude`) — used for looop's per-beat
decide (`tick`) and to launch workers. looop is a single self-contained binary —
spawning, listing, killing and pruning sessions all run in-process, no extra
executable required.
Sessions are stored under `$LOOOP_DATA_DIR/sessions`, self-contained per profile:
looop sets no extra environment and shares no global state, and session ids are
bare (the pulse is `pulse`). (Workers that touch code also need `git` or `box`
to sandbox themselves, but that's a worker concern.)

## Config & data

- **Config** — `$LOOOP_DATA_DIR/config.json` (override `LOOOP_CONFIG`). Lives
  inside the data dir so a profile is fully self-contained. One file: runner
  wiring (a `tick` command for the per-beat decide and an `interactive` command
  to launch workers, per runner) plus the pulse `interval` and optional
  `max_daily_usd` budget. Default runner is `pi`; `claude` is built in.
- **Data / memory** — `$XDG_STATE_HOME/looop/` (override `LOOOP_DATA_DIR`). A
  plain directory holding the PLAYBOOK, goals, journal, and sensors. looop does
  not version it for you — `git init` the data dir yourself if you want history
  and rollback of your policy files. Worker and pulse sessions live under
  `sessions/` in the same dir, so a profile is fully self-contained. Pointing
  `LOOOP_DATA_DIR` elsewhere gives you an isolated **profile** with its own
  sessions.

LLM spend is recorded in an append-only ledger — looop meters its own per-beat
decide in-process, and workers self-report via `looop _ cost`; see `looop cost`.
Set `max_daily_usd` in the config to arm a daily budget breaker that skips the AI
once today's spend hits the cap (clears at local midnight).
