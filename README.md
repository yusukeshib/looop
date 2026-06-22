# looop

A tiny, portable control plane for agent-driven work.

`looop` watches the things you care about (GitHub, Linear, Grafana, …) and runs
your worker fleet. The **judgment lives outside looop**: a *root agent* (a
pi/claude session looop starts) reads the world and decides the next move,
driving looop through a small set of verbs. looop itself never calls an LLM — it
is the unbreakable plumbing; the root agent is the brain. One self-contained
binary, no database, no server.

![looop running a tick](demo.png)

## How it works

Three parts. looop senses; a root agent (a pi/claude session YOU run) decides;
workers do the hands-on work.

```
   pulse (looop, no LLM)            root agent (a pi/claude YOU run)   workers
   ─────────────────────           ──────────────────────────   ───────
   sense every beat,                while: `looop _ wait --json`       real agents
   keep snapshots fresh       ◄───  (blocks till something to do)      doing multi-
   (that's all it does)             decide ONE move → `looop _ …`  ──▶  step work
                                    answer asks / relay to you        `ask` + wait
```

1. **SENSE** — the pulse runs every `sensors/*.sh` each beat (cheap, no LLM),
   keeping `snapshots/` fresh. That is all it does.
2. **WAIT** — the root agent blocks on `looop _ wait --json`, which returns the
   moment a worker raises an `ask` or the world changes. looop does not push to
   the agent; the agent pulls. (No daemon poking anything, no attach.)
3. **DECIDE & ACT** — the agent picks ONE move and drives looop via verbs: write
   a goal/sensor/PLAYBOOK, run one gated shell command, start a worker, or
   `looop _ answer` a worker's ask.
4. **HUMAN** — you talk to that agent directly (you launched it); it relays
   anything that needs you and never does irreversible things without your yes.

State lives entirely in files (goals, snapshots, journal, mailbox), so it is
**level-triggered**: the pulse re-senses from scratch every beat and a crashed
pulse just re-reads the unanswered asks on restart.

The human-in-the-loop path is a durable **mailbox**, not a tmux prompt: a worker
that needs a decision runs one blocking `looop _ ask …` and waits; the root agent
sees it in `looop _ wait`, answers it (or relays to you) with `looop _ answer`.
No attach, no stdin wrangling — it works for headless workers.

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
| `asks/` `answers/` | the worker ↔ root-agent mailbox (questions + answers)           |

**Workers** are the hands. When a move needs real, multi-step work, the root
agent spawns an agent session that runs detached, in parallel, and reconciles its
task on its own. Workers that touch code provision their own sandbox first; looop
itself knows nothing about repos.

**Humans in the loop.** You talk to the *root agent* directly — it is a pi/claude
session YOU launched — never to workers. A worker that needs a decision runs
`looop _ ask` and blocks; the root agent sees it in `looop _ wait`, answers it or
relays the question to you in chat and replies with `looop _ answer`. Irreversible
actions (merges, deploys, deletes) always require your explicit approval in chat.

## Quick start

```sh
looop up            # start the pulse (the sensing loop), detached
# then, in another window, start your agent and point it at looop:
pi                  # then say: "observe looop — loop on `looop _ wait --json`
                    #            and act on it; read `looop --help` first"
looop down          # stop the pulse and all workers
```

`looop up` starts only the pulse. The judgment is a pi/claude session **you**
run and tell to observe looop — looop does not launch or manage it. There is no
bare-`looop` foreground mode and no attach: the agent pulls state with
`looop _ wait`, and you talk to that agent in its own window. `looop up --json`
makes the pulse log machine-readable NDJSON.

On the first run looop seeds a starter PLAYBOOK and a `setup` goal whose only job
is for the root agent to **interview you** and rewrite the PLAYBOOK, goals, and
sensors to match your real work. After that it just runs.

## Commands

```sh
# HUMAN (that's nearly all you run)
looop up [--json]              start the pulse (sensing loop, detached)
looop down                     stop the pulse and all workers
looop cost [today|all|--json]  report LLM spend (agents self-report via `_ cost`)
looop config zsh|bash          print shell integration (tab completions)
looop version | help           (looop help = the full design manual + contract)

# ROOT AGENT (the pi/claude session YOU run) — see the CONTRACT in `looop help`
looop _ wait [--json]                      block until something to act on, then read
looop _ state [--json]                     read current world state (one-shot)
looop _ answer <ask_id> "<text>"           resolve a worker's pending ask
looop _ goal write <id> [body|stdin] | _ goal archive <id>
looop _ sensor write <name> [script|stdin] | _ playbook write [body|stdin]
looop _ run <cmd…> [--reason T]            ONE reversible shell command
looop _ worker start <id> <prompt…> | _ worker kill <id>
looop _ notify <message…>                  surface a notice to the human

# WORKER self-callbacks (auto-injected contract — not human commands)
looop _ ask <id> --prompt "…" [--ref P] [--options a,b]   ask + block for answer
looop _ kill <id> | _ claim <name> | _ unclaim <name> | _ cost <…>
```

The human surface is tiny — essentially `up`/`down` (plus `cost`/`config`). You
run your own agent and tell it to observe looop; everything else you do by
talking to that agent. The `looop _ …` verbs are machine-facing: the **root
agent** drives the world (wait, state, answer, goal, sensor, playbook, run,
worker, notify), and **workers** self-report (ask, kill, claim, unclaim, cost)
via the auto-injected contract.

## Shell integration

```sh
# Zsh (~/.zshrc)
eval "$(looop config zsh)"

# Bash (~/.bashrc)
eval "$(looop config bash)"
```

This adds tab completion for looop's (small) human command surface.

To change judgment: have the root agent run `looop _ playbook write` (or edit a
goal) — it takes effect next beat.

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

Runtime deps: just an LLM runner (`pi` or `claude`) — used to launch the root
agent and workers. looop is a single self-contained binary — spawning, listing,
killing and pruning sessions all run in-process, no extra executable required.
Sessions are stored under `$LOOOP_DATA_DIR/sessions`, self-contained per profile:
looop sets no extra environment and shares no global state, and session ids are
bare (the pulse is `pulse`). (Workers that touch code also need `git` or `box`
to sandbox themselves, but that's a worker concern.)

## Config & data

- **Config** — `$LOOOP_DATA_DIR/config.json` (override `LOOOP_CONFIG`). Lives
  inside the data dir so a profile is fully self-contained. One file: runner
  wiring (an `interactive` command per runner) plus the pulse `interval`. Default
  runner is `pi`; `claude` is built in.
- **Data / memory** — `$XDG_STATE_HOME/looop/` (override `LOOOP_DATA_DIR`). A
  plain directory holding the PLAYBOOK, goals, journal, and sensors. looop does
  not version it for you — `git init` the data dir yourself if you want history
  and rollback of your policy files. Worker, pulse and root-agent sessions live
  under `sessions/` in the same dir, so a profile is fully self-contained.
  Pointing `LOOOP_DATA_DIR` elsewhere gives you an isolated **profile** with its
  own sessions.

LLM spend is recorded in an append-only ledger when agents (workers and the root
agent) self-report via `looop _ cost`; see `looop cost`. looop runs no LLM of its
own, so there is no tick metering or daily-budget breaker — cost control lives in
whatever harness runs your root agent.
