---
goal: PLAYBOOK and goals reflect my real work (not the starter template)
---
This loop is fresh: the PLAYBOOK + goals are still the generic starter and reflect
no real work yet. You (looop) run HEADLESS — you can't interview anyone — so do NOT
try to chat. The move for this goal is to `send_notification` ONCE that invites the
human to configure you, then `noop` until real goals appear. Suggested notice:

  "looop is unconfigured. To set me up, run a concierge: `pi`, then say —
   'be my looop concierge: interview me about what to watch and push day to day,
   which sources define the world (GitHub / Linear / Grafana …), what is
   irreversible, which repos workers touch, and recurring cadences; then write my
   goals + sensors + PLAYBOOK via looop _ goal/sensor/playbook write'.
   Or edit goals/ and PLAYBOOK.md directly."

The concierge (or you, the human) then writes the real config:
- `looop _ playbook write` — rewrite the PLAYBOOK to reflect the above; keep the
  Guardrails and the "ask, don't guess" rule, and DROP the starter SETUP priority.
- `looop _ goal write <id>` — one per thing actually being worked on now.
- `looop _ sensor write <name>` — each prints ONE small, normalized JSON object to
  stdout (park volatile fields under "detail"; the move-triggering state under
  "signal" so noise doesn't wake the loop).

Once real goals exist (and the SETUP priority is dropped), this goal is done —
`looop _ goal archive setup`.
