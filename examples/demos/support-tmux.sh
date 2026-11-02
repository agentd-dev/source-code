#!/usr/bin/env bash
# The on-camera layout for demo (c): the three channels' TUIs side by side,
# and under them one pane with each agent's `instruction.applied` as it is
# logged, in a fixed-size tmux window: 120x32 by default, or the size in
# SUPPORT_COLS x SUPPORT_ROWS, with SUPPORT_WATCH_ROWS rows (default 8) for
# the log pane. A taller window lets a long reply show from its first line.
#
#   . examples/demos/env.sh
#   examples/demos/support-tmux.sh          # then: tmux attach -t support
#
#   ┌ chat (39 cols) ────┬ voice (40 cols) ────┬ email (39 cols) ────┐
#   │ agentd-tui :8430   │ agentd-tui :8431    │ agentd-tui :8432    │
#   ├────────────────────┴─────────────────────┴─────────────────────┤
#   │ 23:01:07.412  chat   instruction.applied  v1… → v2…            │
#   └───────────────────────── (8 rows) ─────────────────────────────┘
#
# Start the agents first (README.md, demo (c)), each writing its log to
# $AGENTD_DEMO_HOME/support-<channel>.log. TUI= picks the client (default:
# agentd-tui on PATH, else this repository's built interface). The session
# is started detached with its size fixed, so attaching from a terminal of
# another size does not resize what is recorded. Panes are addressed by id,
# so a tmux.conf with another base-index changes nothing.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
: "${AGENTD_DEMO_HOME:?source examples/demos/env.sh first}"
TOKEN="$AGENTD_DEMO_HOME/tokens/support.token"
[ -s "$TOKEN" ] || { echo "no $TOKEN: source examples/demos/env.sh first" >&2; exit 2; }
if [ -z "${TUI:-}" ]; then
  if command -v agentd-tui >/dev/null; then TUI=agentd-tui; else TUI="node $ROOT/interface/dist/tui/cli.js"; fi
fi
SESSION="${SESSION:-support}"
COLS="${SUPPORT_COLS:-120}"
ROWS="${SUPPORT_ROWS:-32}"
WATCH_ROWS="${SUPPORT_WATCH_ROWS:-8}"
# Three columns split the width; two cells go to the borders between them.
col=$(( (COLS - 2) / 3 ))
tui() { echo "$TUI --endpoint http://127.0.0.1:$1 --bearer-file $TOKEN"; }
logs="chat=$AGENTD_DEMO_HOME/support-chat.log voice=$AGENTD_DEMO_HOME/support-voice.log email=$AGENTD_DEMO_HOME/support-email.log"

tmux kill-session -t "$SESSION" 2>/dev/null || true
chat=$(tmux new-session -d -P -F '#{pane_id}' -s "$SESSION" -x "$COLS" -y "$ROWS" "$(tui 8430)")
tmux set-option -t "$SESSION" window-size manual
tmux resize-window -t "$SESSION" -x "$COLS" -y "$ROWS"
tmux set-option -t "$SESSION" status off
tmux set-option -t "$SESSION" pane-border-status top
tmux set-option -t "$SESSION" pane-border-format ' #{pane_title} '
watch=$(tmux split-window -P -F '#{pane_id}' -t "$chat" -v -l "$WATCH_ROWS" \
  "node $HERE/standins/watch.mjs --events instruction.applied $logs")
voice=$(tmux split-window -P -F '#{pane_id}' -t "$chat" -h -l $(( COLS - col - 1 )) "$(tui 8431)")
email=$(tmux split-window -P -F '#{pane_id}' -t "$voice" -h -l "$col" "$(tui 8432)")
tmux select-pane -t "$chat" -T 'chat · 127.0.0.1:8430'
tmux select-pane -t "$voice" -T 'voice · 127.0.0.1:8431'
tmux select-pane -t "$email" -T 'email · 127.0.0.1:8432'
tmux select-pane -t "$watch" -T 'instruction.applied, as each agent logs it'
tmux select-pane -t "$chat"
echo "tmux session '$SESSION' is ready (${COLS}x${ROWS}): tmux attach -t $SESSION"
echo "panes: chat=$chat voice=$voice email=$email watch=$watch"
