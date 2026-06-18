# looop

A tiny, portable, Kubernetes-shaped control loop for your work.

`looop` watches the things you care about (GitHub, Linear, Grafana, …), and once
per beat asks an LLM to make **exactly one move** toward your goals — then stops.
It's a single self-contained bash script with no daemon, no database, no server.

![looop running a tick](demo.png)

One full beat (sense → decide → journal), then the next beat skips the LLM
entirely because nothing in the world changed.

## How it works

Like a Kubernetes controller, every **tick** reconciles *desired state* against
*observed state* and takes one step to close the gap:

```
       ┌─────────────────────────────────────────────┐
       │  sense → diff → decide ONE move → act → log   │
       └─────────────────────────────────────────────┘
                          one tick

1. SENSE    run every sensors/*.sh → each prints one JSON snapshot of the world
2. DIFF     hash (goals + snapshots + workers). Unchanged since last tick?
            → skip, no LLM call (cheap, level-triggered)
3. DECIDE   hand the PLAYBOOK + goals + snapshots + live workers to the LLM;
            it picks THE single most important move
4. ACT      a small reversible action, edit a goal/sensor, or start a worker
5. LOG      append one line to journal.md, surface anything that needs you
```

Each tick is **stateless and disposable**: the process carries nothing in
memory between beats — all state lives in files (goals, snapshots, journal,
claims). Because of that the loop is **level-triggered**, not edge-triggered:
every tick re-derives what to do from the *current* world (snapshots are wiped
and re-sensed each beat), so a crashed tick, renamed sensor, or dead worker just
self-heals on the next beat. Kill the pulse anytime; the next tick picks up
exactly where the world is, not where a remembered cursor left off.

## Concepts

Everything lives as plain files in the data dir (a git repo = the loop's memory):

| File / dir      | Role (Kubernetes analogy)                                          |
| --------------- | ------------------------------------------------------------------ |
| `PLAYBOOK.md`   | the controller logic — your judgment, priorities, guardrails       |
| `goals/*.md`    | desired state — one declarative spec per thing you're pushing      |
| `sensors/*.sh`  | observers — each prints **one JSON object** describing the world   |
| `journal.md`    | the action log — one line per move                                 |
| `claims/`       | leases — a worker writes one to *own* a task; stale ones auto-reap |
| `reports/`      | deliverables a human reads (persists across ticks)                 |

**Workers** are the hands. When a move needs real, multi-step work, the loop
spawns an agent session (via [`babysit`](https://github.com/yusukeshib/babysit))
that runs detached, in parallel, and reconciles its task on its own. Workers
that touch code provision their own sandbox first; the loop itself knows nothing
about repos.

**Humans in the loop.** Workers never guess and never send OS notifications.
When one needs a decision it raises a flag and waits; the pulse pops a tmux
window you can't miss. You attach, answer, and it continues. Irreversible
actions (merges, deploys, deletes) always require your explicit approval.

## Quick start

```sh
looop          # run the pulse (foreground; Ctrl-C to stop)
```

On the first run the loop seeds a starter PLAYBOOK and a `setup` goal whose only
job is to **interview you** and rewrite the PLAYBOOK, goals, and sensors to match
your real work. After that it just runs.

## Commands

```sh
looop                       run the pulse (default; ticks on a cadence)
looop tick                  run a single beat and exit (debug / cron)
looop run <goal-id>         force ONE move for a goal NOW (manual override)
looop ls [--watch]          list this profile's worker sessions (⚑ = waiting)
looop cost [today|--json]   report LLM spend from the cost ledger
looop version | help
```

To talk to a waiting worker: `babysit attach -s looop-<id>`.
To pause the loop: drop a file at `$data/paused`. To change judgment: edit
`PLAYBOOK.md` — it takes effect next tick.

## Install

### curl (recommended)

```sh
curl -fsSL https://raw.githubusercontent.com/yusukeshib/looop/main/install.sh | bash
```

Downloads `looop` to `~/.local/bin/looop`. Override with `LOOOP_INSTALL_DIR`, or
pin a ref with `LOOOP_REF`:

```sh
LOOOP_INSTALL_DIR=/usr/local/bin LOOOP_REF=v0.2.0 \
  curl -fsSL https://raw.githubusercontent.com/yusukeshib/looop/main/install.sh | bash
```

Make sure the install dir is on your `PATH` (the installer warns you if not):

```sh
export PATH="$HOME/.local/bin:$PATH"
```

### Nix (flakes)

```sh
nix run github:yusukeshib/looop                 # run without installing
nix profile install github:yusukeshib/looop     # install into your profile
nix develop github:yusukeshib/looop             # dev shell with runtime deps
```

### Manual

```sh
git clone https://github.com/yusukeshib/looop.git
ln -s "$PWD/looop/looop" ~/.local/bin/looop
```

### Verify

```sh
looop version   # -> looop 0.11.1
looop help
```

Runtime deps: `bash`, `jq`, `babysit`, and an LLM runner (`pi` or `claude`).
(Workers that touch code also need `git` or `box` to sandbox themselves, but
that's a worker concern, not a prerequisite for the pulse.)

## Config & data

- **Config** — `$XDG_CONFIG_HOME/looop.json` (override `LOOOP_CONFIG`). One file:
  runner wiring and tick cadence. Default runner is `pi`; `claude` is built in.
- **Data / memory** — `$XDG_STATE_HOME/looop/` (override `LOOOP_DATA_DIR`). A git
  repo holding the PLAYBOOK, goals, journal, and sensors. Pointing
  `LOOOP_DATA_DIR` elsewhere gives you an isolated **profile** with its own
  worker fleet.

LLM spend is metered automatically (ticks, manual runs, and self-reporting
workers) into an append-only ledger; see `looop cost`.
