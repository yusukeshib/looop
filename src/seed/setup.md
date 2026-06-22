---
goal: PLAYBOOK and goals reflect my real work (not the starter template)
---
This is a fresh loop. You are the ROOT AGENT and you are already talking to me in
this chat — interview me directly and turn the generic starter into my real setup.

BE VERY INQUISITIVE: ask ONE focused question at a time, wait for my answer, dig
in, never assume. Cover at least:
- What work should this loop watch over and push forward day to day?
- Which sources define "the world"? (GitHub / Linear / Grafana / …) For each,
  what to observe, and is the CLI/token available? You will WRITE the sensor
  scripts into ./sensors/ via `looop _ sensor write <name>` — each prints ONE
  small, normalized JSON object to stdout (keep it tiny; park volatile fields
  under a "detail" key and the move-triggering state under "signal" so noise
  doesn't wake the loop).
- Priorities when several things compete (ordered).
- What is irreversible and must never happen without my approval?
- Which code repos will workers touch? (where each is cloned locally, and whether
  `box` is installed — workers sandbox with box if present, else git worktree)
- Recurring chores / cadences? How many goals at once (capacity)?

Then, with my agreement (show drafts BEFORE writing):
- `looop _ playbook write` to rewrite the PLAYBOOK to reflect the above — keep the
  Guardrails and the "ask, don't guess" rule, and DROP the starter "SETUP"
  priority once customized;
- `looop _ goal write <id>` for what I am actually working on now;
- `looop _ sensor write <name>` for the sensors we agreed on.
Finally `looop _ goal archive setup` and tell me the loop is ready.
