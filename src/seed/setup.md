---
goal: PLAYBOOK and goals reflect my real work (not the starter template)
---
This loop is fresh: the PLAYBOOK + goals are still the generic starter and reflect
no real work yet. You (looop) run HEADLESS — you can't interview anyone — so do NOT
try to chat. The fresh data dir already contains a real pending `setup` ask so a
client/concierge waiting on asks wakes immediately and starts the interview. While
that ask is pending, `noop` quietly until real goals appear.

The client (or you, the human) then writes the real config:
- `looop playbook write` — rewrite the PLAYBOOK to reflect the above; keep the
  Guardrails and the "ask, don't guess" rule, and DROP the starter SETUP priority.
- `looop goal write <id>` — one per thing actually being worked on now.
- `looop sensor write <name>` — each prints ONE small, normalized JSON object to
  stdout (park volatile fields under "detail"; the move-triggering state under
  "signal" so noise doesn't wake the loop).

Once real goals exist (and the SETUP priority is dropped), answer the starter
`setup-1` ask with a short note like "setup complete" and archive this goal:
`looop goal archive setup`.
