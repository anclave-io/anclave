#!/bin/sh
# Drive built binaries the way a person does: start the daemon, create a
# session, type into it, and read the screen back.
#
# This exists because 0.2.0 shipped with `session send` delivering nothing and
# `session capture` returning a blank screen. Both passed a green suite: the
# tests asserted the calls returned rather than that anything happened. A
# release is not verified by `--version`.
#
# Usage: scripts/smoke.sh [BINDIR]   (default: target/release)
set -eu

bindir=${1:-target/release}
case "$bindir" in
  /*) ;;
  *) bindir="$PWD/$bindir" ;;
esac

for binary in anclaved anclave-cli; do
  [ -x "$bindir/$binary" ] || { echo "smoke: $bindir/$binary is not executable" >&2; exit 1; }
done
command -v tmux >/dev/null 2>&1 || { echo "smoke: tmux is required" >&2; exit 1; }

# Short path on purpose: a Unix socket path is bounded near 104 bytes, and a
# mktemp directory under a runner's TMPDIR can exceed that on macOS.
sock="/tmp/anclave-smoke-$$.sock"
log="/tmp/anclave-smoke-$$.log"
marker="smoke-ok-$$"
daemon_pid=""

cleanup() {
  [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
  rm -f "$sock" "$log"
}
trap cleanup EXIT INT TERM

ANCLAVE_SOCKET="$sock"
export ANCLAVE_SOCKET

"$bindir/anclaved" --socket "$sock" >"$log" 2>&1 &
daemon_pid=$!

ready=""
i=0
while [ "$i" -lt 50 ]; do
  if "$bindir/anclave-cli" daemon status >/dev/null 2>&1; then ready=yes; break; fi
  i=$((i + 1))
  sleep 0.2
done
[ -n "$ready" ] || { echo "smoke: daemon never became reachable" >&2; cat "$log" >&2; exit 1; }
echo "smoke: daemon is up"

"$bindir/anclave-cli" session create smoke >/dev/null
echo "smoke: session created"

# The screen must show something. A blank capture for a live pane is the
# 0.2.0 bug: the capture's last newline scrolled the only content row away.
screen=""
i=0
while [ "$i" -lt 50 ]; do
  screen=$("$bindir/anclave-cli" session capture session-0 2>/dev/null || true)
  case "$screen" in
    *[!\ ]*)
      # Any non-space glyph inside the row text means the pane rendered.
      if printf '%s' "$screen" | tr -d ' \n\r\t' | grep -q '[$#>%]'; then break; fi
      ;;
  esac
  i=$((i + 1))
  sleep 0.2
done
printf '%s' "$screen" | tr -d ' \n\r\t' | grep -q '[$#>%]' || {
  echo "smoke: the session's screen never showed a prompt" >&2
  echo "smoke: distinct row contents follow" >&2
  printf '%s\n' "$screen" | grep -o '"text": "[^"]*"' | sort -u | head -5 >&2
  exit 1
}
echo "smoke: screen renders"

# Multi-byte input must arrive. `send-keys -H` exits 0 for a malformed
# argument shape, so this is only observable by reading the screen back.
"$bindir/anclave-cli" session send session-0 "echo $marker
" >/dev/null

seen=""
i=0
while [ "$i" -lt 75 ]; do
  if "$bindir/anclave-cli" session capture session-0 2>/dev/null | grep -q "$marker"; then
    seen=yes
    break
  fi
  i=$((i + 1))
  sleep 0.2
done
[ -n "$seen" ] || { echo "smoke: input never reached the agent" >&2; exit 1; }
echo "smoke: input round-trips"

"$bindir/anclave-cli" session delete session-0 >/dev/null 2>&1 || true
echo "smoke: ok"
