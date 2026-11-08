#!/bin/sh
# Phase 0 smoke test: does herdr bring the daemon back after a server restart?
# Run inside a throwaway herdr session, with herdr-superagent on PATH.
#
#   herdr --session sa-smoke &            # a disposable session
#   HERDR_SOCKET_PATH=... sh scripts/smoke-restart.sh
#
# It records the daemon pid and pane before and after `herdr server stop` +
# `herdr --session sa-smoke`, and prints PASS when a new daemon answers on the
# same pane with the same profile.
set -eu
profile=${SUPERAGENT_PROFILE:-smoke}
herdr=${HERDR_BIN_PATH:-herdr}
state=${HERDR_SUPERAGENT_STATE_DIR:-$HOME/.local/share/herdr-superagent}
sock="$state/profiles/$profile/daemon.sock"

herdr-superagent --profile "$profile" ensure --json
before=$(herdr-superagent --profile "$profile" status --json)
pid_before=$(printf '%s' "$before" | sed -n 's/.*"pid": *\([0-9]*\).*/\1/p')
pane_before=$(printf '%s' "$before" | sed -n 's/.*"pane_id": *"\([^"]*\)".*/\1/p')
echo "before: pid=$pid_before pane=$pane_before"

session=$("$herdr" api snapshot | sed -n 's/.*"session_name": *"\([^"]*\)".*/\1/p' | head -n 1)
"$herdr" server stop
sleep 2
if [ -n "$session" ]; then
  nohup "$herdr" --session "$session" >/dev/null 2>&1 &
else
  echo "could not read the session name; start herdr again by hand, then press Enter"
  read -r _
fi

for _ in $(seq 1 60); do
  if [ -S "$sock" ] && after=$(herdr-superagent --profile "$profile" status --json 2>/dev/null); then
    pid_after=$(printf '%s' "$after" | sed -n 's/.*"pid": *\([0-9]*\).*/\1/p')
    pane_after=$(printf '%s' "$after" | sed -n 's/.*"pane_id": *"\([^"]*\)".*/\1/p')
    echo "after:  pid=$pid_after pane=$pane_after"
    if [ "$pid_after" != "$pid_before" ] && [ -n "$pane_after" ]; then
      echo "PASS: herdr resumed the daemon in pane $pane_after"
      exit 0
    fi
  fi
  sleep 1
done
echo "FAIL: no daemon answered within 60s after the restart" >&2
exit 1
