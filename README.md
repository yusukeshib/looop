# looop

A tiny, portable, autonomous control loop for agent-driven work. One
self-contained binary — no database, no server, no helper files.

## The idea

**looop is the brain, not a task runner.** It watches the things you care about
(GitHub, Linear, Grafana, …) and runs a fleet of worker agents. Each beat it
senses the world and, if something changed, decides the *single* most important
move and executes it — including spawning workers. The judgment lives *inside*
looop (a small, gated LLM call per beat).

An autonomous loop is easy. The hard part — and the whole point of looop's
design — is **where and how a human enters the loop.** Too much human and it
isn't autonomous; too little and it's reckless. looop's answer is to pull you in
at exactly two kinds of moments, and nowhere else.

## How the human stays in the loop

There are two distinct ways you touch the loop — and that split *is* the design.

**Steer — async, you initiate.** You are a peer, not a driver. You shape *what*
looop pursues by editing goals and the PLAYBOOK; it observes them next beat. This
never blocks the loop — you set direction and walk away.

```sh
looop goal write ship-v2 -      # declare desired state (effective next beat)
looop playbook write -          # your judgment, priorities, guardrails
looop tell ship-v2 "skip the docs, focus the API"   # steer a RUNNING worker
looop schedule write digest --every 86400 --note "daily digest"  # durable timer
```

**Answer — sync, the loop initiates.** looop reaches back for *you* only when it
genuinely must: a worker hits a decision only a human can make, or an
irreversible action — merge, deploy, delete — needs an explicit yes. It blocks
and waits for your call.

```sh
looop wait --only-asks          # block cheaply until the loop needs you
looop answer <id> "yes"         # unblock the worker / approve the gate
```

The key move: **the intervention point is decoupled from any UI.** Asks and
answers are a durable file mailbox reached through one backend-agnostic contract
(`looop …`), so the loop never blocks on a particular terminal, tmux, or stdin
— it just needs an answer *eventually*, from whatever channel reaches you:

- a **bare terminal** — you typing the verbs yourself (the thinnest client);
- an **agent concierge** — a `claude`/`codex`/`opencode`/`pi` session that relays
  asks in plain language and answers on your behalf;
- a **notify script** — a loop that pushes asks to Slack/SMS and relays your reply.

A client is an *interface*, never a decision-maker. looop decides; the client
just carries the question to you and your answer back.

Two properties make all this dependable:

- **Level-triggered.** All state is plain files, so the loop re-senses every beat
  and a crashed pulse just re-reads its files on restart. A pending ask survives
  restarts — no queues, no lost work.
- **One move per beat.** Each beat does at most one thing. Behavior stays legible
  and cheap — an unchanged world costs no LLM call, and a repeatedly-failing beat
  backs off exponentially instead of burning retries.

## One beat: sense → decide → act

1. **SENSE** — run every `sensors/*.sh` (concurrently; a script may declare
   `# looop:interval=<secs>` to skip beats while its snapshot is fresh),
   refreshing `snapshots/`. World unchanged since last beat → stop here, no LLM
   call — unless the last decision was a `noop` older than `LOOOP_NOOP_TTL`
   (default 6h), which re-decides so one wrong noop can't park a world forever.
2. **DECIDE** — on change, hand the PLAYBOOK + goals + readings + pending asks to
   the LLM — plus a computed **WHAT CHANGED** diff (why it was woken) and, after
   a failed beat, a **LAST FAILURE** section (so it corrects instead of
   re-emitting the same failing move) — which returns **one** typed move.
3. **ACT** — execute it: write a goal/sensor/PLAYBOOK, run one reversible command,
   or spawn a worker. Irreversible moves are gated — they wait for your `answer`
   (see above), and so does any worker that hits a human-only decision.

## Three layers

| Layer        | What it is                                                            |
| ------------ | --------------------------------------------------------------------- |
| **core**     | the autonomous pulse + the durable state behind it. Decides and acts. |
| **contract** | the `looop …` verbs — the one stable, backend-agnostic surface to read and steer core. |
| **client**   | anything that drives the contract for a human (terminal / concierge / notify). An interface, never a decision-maker. |

State is plain files in the data dir, reached *through* the contract — not a
public interface:

| File / dir         | Role                                                    |
| ------------------ | ------------------------------------------------------- |
| `PLAYBOOK.md`      | your judgment, priorities, guardrails                   |
| `goals/*.md`       | desired state — one declarative spec per thing you push |
| `sensors/*.sh`     | observers — each prints **one JSON object**             |
| `journal.md`       | action log — one line per move                          |
| `asks/` `answers/` | the worker ↔ human mailbox                              |
| `tells/`           | steering messages into a running worker (`looop tell`)  |
| `schedules/`       | durable time triggers — one-shot / recurring; due-ness wakes the loop |

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/yusukeshib/looop/main/install.sh | bash
# or
cargo install looop
```

**Only hard dependency:** an LLM runner. `claude` is the default; `codex`,
`opencode`, and `pi` also work — pick one with `looop init`. (Workers that touch
code isolate their own sandbox via `git worktree` — a worker convention, not a
looop dependency.)

## Usage

```sh
looop init     # interactive setup — required before `up`; pick the runner wiring
looop up       # start the autonomous pulse (detached)
looop down     # stop the pulse and all workers
```

The pulse refuses to start until `looop init` writes the runner wiring, so the
agent CLI driving every tick and worker is always an explicit choice. Read and
steer core with the `looop …` verbs (`state`, `wait`, `answer`,
`goal write`), by hand or through a client.

### First run

looop runs headless, so it can't interview you. A fresh data dir is seeded with a
starter PLAYBOOK, a `setup` goal, and a real pending `setup` ask so a client
waiting on asks wakes immediately.

Start with `looop up`, then answer asks by hand (`looop asks`, `looop answer`)
or through a client. Answer the starter `setup` ask, edit your goals/PLAYBOOK
with the `looop …` verbs, archive the `setup` goal, and looop runs from there.

**Even easier: an agent concierge.** Point a `claude`/`codex`/`opencode`/`pi`
session at looop and talk to it in plain language — it relays asks with
recommendations, drives the write verbs for you, and interviews you to shape your
goals, sensors, and PLAYBOOK:

```sh
claude   # then say:
# "be my looop concierge: run `looop up`, relay the setup goal, and interview
#  me to write my goals, sensors, and PLAYBOOK."
```

See `looop help` for the full command reference and design manual.

## Shell integration

looop ships completions for zsh and bash. Enable them by evaluating the output of
`looop config <shell>` from your shell rc:

```sh
# ~/.zshrc
eval "$(looop config zsh)"

# ~/.bashrc
eval "$(looop config bash)"
```

Completion covers every subcommand and dynamically completes live ids read
straight from the data dir — pending asks for `looop answer`, goal ids for
`looop goal`, sensor names for `looop sensor write`, worker/session ids for
`looop worker kill` / `kill` / `screenshot`, and lease names for `claim` /
`unclaim`. It honors `$LOOOP_DATA_DIR` (then the XDG default), so per-profile
data dirs complete correctly.

## Configuration

The config (`$LOOOP_CONFIG`, default `~/.config/looop/config.json`) is just **two
shell commands**. `looop init` lets you pick `claude`, `codex`, `opencode`, `pi`,
or `custom`; after that looop treats the result as plain runner wiring:

| Key              | Role                                                                                     |
| ---------------- | ---------------------------------------------------------------------------------------- |
| `tick_command`   | run ONE disposable decision. The prompt is passed via the `{{prompt_file}}` placeholder (substituted with the prompt file path — read it with `$(cat {{prompt_file}})` or `@{{prompt_file}}`). If you omit the placeholder the prompt is piped in on **stdin** instead. Must run unattended (no permission prompts — the detached pulse can't answer them) and emit a structured event stream looop can render. |
| `worker_command` | launch a worker agent. Same `{{prompt_file}}` placeholder, substituted with the worker's prompt file path. (A worker can't use the stdin fallback — stdin is its live attach TTY.) |

**Per-worker command override.** `looop worker start <id> … --command "…"` (and the decider's `start_worker.command` field) replaces the `worker_command` template **wholesale** for that one worker — a different runner, model, or flags. The override is a full launch command and must contain `{{prompt_file}}`, exactly like the template:

```sh
looop worker start heavy-refactor "…brief…" \
  --command 'pi --model claude-opus-4-8 --thinking high @{{prompt_file}}'
```

looop itself has **no runner vocabulary** — which flags mean "model" or "effort" is the runner's business, decided at `looop init` time or per-worker via the override. Policy for *when* to override belongs in your PLAYBOOK (with exact commands valid on your machine); without such guidance the decider always uses the configured template.

> **Removed in 1.0:** the `{{model}}`/`{{thinking}}` placeholders, the `worker_model`/`worker_thinking` config keys, and the `--model`/`--thinking` flags. A `worker_command` still carrying those placeholders is refused at launch — re-run `looop init` (or edit the config) to bake the values into the command.

The built-in presets are:

**claude** (default)

```json
{
  "tick_command": "claude -p --output-format stream-json --verbose --dangerously-skip-permissions --model sonnet \"$(cat {{prompt_file}})\"",
  "worker_command": "claude --dangerously-skip-permissions --model opus \"$(cat {{prompt_file}})\""
}
```

**codex**

```json
{
  "tick_command": "codex exec --json --dangerously-bypass-approvals-and-sandbox \"$(cat {{prompt_file}})\"",
  "worker_command": "codex --dangerously-bypass-approvals-and-sandbox \"$(cat {{prompt_file}})\""
}
```

**opencode** (best-effort — verify against your installed version)

```json
{
  "tick_command": "opencode run \"$(cat {{prompt_file}})\"",
  "worker_command": "opencode \"$(cat {{prompt_file}})\""
}
```

**pi**

```json
{
  "tick_command": "pi -p --mode json -ne --model claude-sonnet-4-5 --thinking low @{{prompt_file}}",
  "worker_command": "pi --model claude-opus-4-8 --thinking medium @{{prompt_file}}"
}
```

Model ids above are examples. For claude, `sonnet`/`opus` are aliases that always
resolve to the latest of each; pin a specific version (e.g.
`--model claude-opus-4-1`) if you need reproducibility.
