# looop

A tiny, portable control plane for agent-driven work. One self-contained binary —
no database, no server.

## What it does

`looop` watches the things you care about (GitHub, Linear, Grafana, …) and runs a
fleet of worker agents. Every beat it senses the world and, if something changed,
makes the single most important move — including spawning workers. You don't drive
it; you steer it by editing goals and the PLAYBOOK. Irreversible actions (merges,
deploys, deletes) always wait for your explicit yes.

## Architecture

Each beat the pulse runs three steps:

1. **SENSE** — run every `sensors/*.sh`, refreshing `snapshots/`. Unchanged world
   → stop here, no LLM call.
2. **DECIDE** — on change, hand PLAYBOOK + goals + readings + asks to the LLM,
   which returns **one** typed move.
3. **ACT** — execute it: write a goal/sensor/PLAYBOOK, run one reversible command,
   or spawn a worker. One move per beat; a daily budget caps spend.

State lives entirely in files, so the loop is **level-triggered**: it re-senses
every beat and a crashed pulse just re-reads its files on restart. When a worker
needs a human decision it blocks on `looop _ ask`; you reply with `looop _ answer`
— a durable mailbox that needs no tmux or stdin.

Everything is plain files in the data dir:

| File / dir         | Role                                                    |
| ------------------ | ------------------------------------------------------- |
| `PLAYBOOK.md`      | your judgment, priorities, guardrails                   |
| `goals/*.md`       | desired state — one declarative spec per thing you push |
| `sensors/*.sh`     | observers — each prints **one JSON object**             |
| `journal.md`       | action log — one line per move                          |
| `asks/` `answers/` | the worker ↔ human mailbox                              |

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/yusukeshib/looop/main/install.sh | bash
# or
cargo install looop
```

**Runtime dep:** an LLM runner — the only hard requirement. `claude` is the
default; `codex`, `opencode`, and `pi` are also supported. Run `looop init` to
pick one (see below).
Workers run in parallel, so each isolates its own workspace (a `git worktree`, or
`box` if available) to avoid clobbering another worker's files; this is a worker
convention, not a dependency of looop itself.

## Usage

```sh
looop init          # interactive setup: pick the runner (claude/codex/opencode/pi)
looop up            # start the autonomous pulse (detached)
looop watch         # live log + running-session selector
looop down          # stop the pulse and all workers
```

`looop init` asks a few questions (runner, then the tick/worker models), each
prefilled with a sensible default (claude → sonnet/opus), and writes the runner
wiring. It is optional: without it the loop runs on the built-in claude defaults,
but running it lets you choose another runner.

### First run

looop is steered by an agent, not by you typing commands. The first-run flow:

1. **`looop init`** — pick the runner (or skip it and accept the claude default).
2. **Start a concierge.** Launch an agent and ask it to drive looop for you:
   ```sh
   claude   # or pi / codex / opencode — then say:
   # "be my looop concierge: run `looop up`, then relay the setup goal and
   #  interview me to write my goals + sensors + PLAYBOOK"
   ```
   The concierge runs `looop up` (starting the autonomous pulse) and speaks plain
   language while driving the `looop _ …` contract for you — relaying pending
   asks, helping edit goals, answering on your behalf.
3. **The first tick opens the `setup` goal.** A fresh data dir is seeded with a
   starter PLAYBOOK + a `setup` goal whose top priority is exactly this: looop
   runs headless (it can't interview anyone), so on the first changed beat it
   journals an invitation that your concierge surfaces, then the concierge
   interviews you and writes your real goals/sensors/PLAYBOOK. Once customized,
   archive the `setup` goal and looop runs from there.

You can also skip the concierge entirely: run `looop up` yourself and steer by
hand (edit goals/PLAYBOOK, or use the `looop _ …` verbs). See `looop help` for the
full command reference and design manual.
