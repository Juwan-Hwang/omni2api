#!/bin/bash
# snapshot the tmux pane (with colours) every 0.2 s; one file per change
last=""
while true; do
  s=$(tmux capture-pane -e -p -t cc 2>/dev/null) || exit 0
  if [ "$s" != "$last" ]; then printf '%s' "$s" > snaps/$(date +%s.%N).ans; last="$s"; fi
  sleep 0.2
done
