---
goal: PLAYBOOK and goals reflect my real work (not the starter template)
---
This loop is fresh: the PLAYBOOK + goals are still the generic starter and reflect
no real work yet. You (looop) run HEADLESS — you can't interview anyone — so do NOT
try to chat. The move for this goal is a `noop` ONCE whose journal line invites the
human to configure you (a client — the pi/claude session the human runs to watch
looop — reads the journal/state and relays it), then `noop` quietly until real
goals appear. Suggested journal line:

  "looop is unconfigured. To set me up, run a client: `pi`, then say —
   'be my looop client: interview me about what to watch and push day to day,
   which sources define the world (GitHub / Linear / Grafana …), what is
   irreversible, which repos workers touch, and recurring cadences; then write my
   goals + sensors + PLAYBOOK via looop _ goal/sensor/playbook write'.
   Or edit goals/ and PLAYBOOK.md directly."

The client (or you, the human) then writes the real config:
- `looop _ playbook write` — rewrite the PLAYBOOK to reflect the above; keep the
  Guardrails and the "ask, don't guess" rule, and DROP the starter SETUP priority.
- `looop _ goal write <id>` — one per thing actually being worked on now.
- `looop _ sensor write <name>` — each prints ONE small, normalized JSON object to
  stdout (park volatile fields under "detail"; the move-triggering state under
  "signal" so noise doesn't wake the loop).

Once real goals exist (and the SETUP priority is dropped), this goal is done —
`looop _ goal archive setup`.
