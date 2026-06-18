---
goal: Every day, the PLAYBOOK is refined from the previous day's journal
---
Recurring goal (never archived). Once per day — when today's date (sensor-today.json)
is newer than the last-improved date below — make ONE small improvement:

- Read the last ~24h of journal.md. Find ONE recurring mistake, hesitation, or
  human intervention and turn it into a clearer PLAYBOOK rule (or sharpen a vague
  one). Keep it to a single small change.
- Do it as an interactive session in the data dir, with a DATE-STAMPED id so a
  finished run never blocks tomorrow's (RULE: recurring goals use dated ids):
     looop start-session playbook-YYYYMMDD "<the task above>"
- PLAYBOOK changes affect every future tick, so PROPOSE the diff and
  `looop flag` for my confirmation before committing.
- When done, set last-improved below to today's date. (The pulse auto-prunes
  finished sessions each tick; `looop prune` clears them on demand.)

last-improved: never
