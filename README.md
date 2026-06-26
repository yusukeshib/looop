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
looop init          # interactive setup: edit the agent commands (tick_command/worker_command)
looop up            # start the autonomous pulse (detached)
looop watch         # live log + running-session selector
looop down          # stop the pulse and all workers
```

`looop init` lets you edit the two command strings of the wiring
(`tick_command` / `worker_command`), each prefilled with the current value (or the
built-in **claude** default on first run). It is **required before `looop up`** —
the pulse refuses to start until the wiring exists, so the agent CLI driving every
tick and worker is an explicit choice rather than a silent default. See
[Configuration](#configuration) for ready-made wirings to paste in.

### First run

looop is steered by an agent, not by you typing commands. The first-run flow:

1. **`looop init`** — accept the claude default, or paste a different runner's
   wiring (see [Configuration](#configuration)). Required before the pulse starts.
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

## Configuration

The config (`$LOOOP_CONFIG`, default `~/.config/looop/config.json`) is just **two
shell commands** — looop is glue and knows nothing about any specific runner:

| Key              | Role                                                                              |
| ---------------- | --------------------------------------------------------------------------------- |
| `tick_command`   | run ONE disposable decision. The tick prompt arrives on **stdin**; must run unattended (no permission prompts — the detached pulse can't answer them) and emit a structured event stream looop can render. |
| `worker_command` | launch a worker agent. `{{prompt_file}}` is substituted with the worker's prompt file path. |

(Re-attaching to a worker is handled in-process by looop, so there is no `resume`
command to configure.) `looop init` just lets you edit these two strings. The
built-in default is `claude`; paste one of the wirings below (or your own) to
switch runner.

**claude** (default)

```json
{
  "tick_command": "claude -p --output-format stream-json --verbose --dangerously-skip-permissions --model sonnet",
  "worker_command": "claude --dangerously-skip-permissions --model opus \"$(cat {{prompt_file}})\""
}
```

**codex**

```json
{
  "tick_command": "codex exec --json --dangerously-bypass-approvals-and-sandbox",
  "worker_command": "codex --dangerously-bypass-approvals-and-sandbox \"$(cat {{prompt_file}})\""
}
```

**opencode** (best-effort — verify against your installed version)

```json
{
  "tick_command": "opencode run",
  "worker_command": "opencode \"$(cat {{prompt_file}})\""
}
```

**pi**

```json
{
  "tick_command": "pi -p --mode json -ne --model claude-sonnet-4-5 --thinking low 'Execute the looop tick instructions provided on stdin.'",
  "worker_command": "pi --model claude-opus-4-8 --thinking medium @{{prompt_file}}"
}
```

Model ids above are examples. For claude, `sonnet`/`opus` are aliases that always
resolve to the latest of each; pin a specific version (e.g.
`--model claude-opus-4-1`) if you need reproducibility.
