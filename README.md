# loop

A tiny, portable, Kubernetes-shaped control loop for your work.

`loop` is a single self-contained bash script. Install by putting it on your
`PATH` (e.g. symlink `~/.local/bin/loop` → this repo's `loop`).

State/memory lives separately in `git@github.com:yusukeshib/loop_state.git`
(cloned to `$XDG_STATE_HOME/loop`). Runner config: `$XDG_CONFIG_HOME/loop.json`.
