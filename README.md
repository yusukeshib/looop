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

**Runtime dep:** an LLM runner (`pi` or `claude`) — the only hard requirement.
Workers run in parallel, so each isolates its own workspace (a `git worktree`, or
`box` if available) to avoid clobbering another worker's files; this is a worker
convention, not a dependency of looop itself.

## Usage

```sh
looop up            # start the autonomous pulse (detached)
looop watch         # live log + running-session selector
looop down          # stop the pulse and all workers
```

On first run, looop seeds a starter PLAYBOOK and a `setup` goal. Replace it with
your real work (edit goals/PLAYBOOK, or use the `looop _ …` steer verbs), and looop
runs from there. See `looop help` for the full command reference and design manual.

The easiest way to steer is a **concierge client** — an agent session that speaks
plain language and drives the `looop _ …` contract for you (relays pending asks,
helps edit goals, answers on your behalf):

```sh
pi   # then: "Work as a concierge for the running `looop` service"
```
