---
goal: PLAYBOOK and goals reflect my real work (not the starter template)
---
This is a fresh loop. Interview me and turn the generic starter into my real
setup. You run as an interactive worker session, in the loop data dir.

BE VERY INQUISITIVE: ask ONE focused question at a time, wait, dig in, never
assume. First raise a flag so I know you are waiting:
  looop flag setup "setup: attach and answer my questions"

Cover at least:
- What work should this loop watch over and push forward day to day?
- Which sources define "the world"? (GitHub / Linear / Grafana / …) For each,
  what to observe, and is the CLI/token available? You WRITE the sensor scripts
  yourself into ./sensors/ — each prints ONE small, normalized JSON object to
  stdout (keep it tiny; park volatile fields under a "detail" key and the
  move-triggering state under "signal" so noise doesn't wake the loop).
- Priorities when several things compete (ordered).
- What is irreversible and must never happen without my approval?
- Which code repos will workers touch? (note where each is cloned locally, and
  whether `box` is installed — workers sandbox with box if present, else git
  worktree)
- Recurring chores / cadences? How many goals at once (capacity)?

Then, with my agreement (show drafts BEFORE writing):
- rewrite ./PLAYBOOK.md to reflect the above — keep the Guardrails and the
  "ask, don't guess" rule, and DROP the starter "SETUP" priority once customized;
- create ./goals/*.md for what I am actually working on now;
- write ./sensors/*.sh we agreed on.
Finally archive this goal (move to goals/archive/), unflag yourself
(looop unflag setup), and tell me the next tick will pick it up.
