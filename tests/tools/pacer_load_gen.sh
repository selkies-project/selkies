#!/bin/bash
# Animated screen damage for encoder load: a large xterm with scrolling text
# moved in a Lissajous pattern so the encoder sees fresh full-rect damage.
# Driven by tests/perf/test_pacer.py; needs xterm and xdotool.
export DISPLAY="${E2E_DISPLAY:?set it to the display the suites drive}"
XPID=""
# Set before anything is started, and whichever branch runs below: a generator
# that took over an existing window still has to take the signal, and the mover
# loop below must never be what is left holding the display.
trap 'if [ -n "${XPID}" ]; then kill "${XPID}" 2>/dev/null || true; fi' EXIT
trap exit INT TERM

# Matched on the class name: -name below sets that, while the window TITLE
# is whatever the program inside the terminal reports ("bash"), so a search
# by name finds nothing and the mover has no window to animate.
WID=$(xdotool search --classname "pacer-load" | head -1)
if [ -z "$WID" ]; then
  # shellcheck disable=SC2016  # the inner shell expands these, not this one
  xterm -geometry 220x45 -name pacer-load -e bash -c 'while true; do for i in $(seq 1 50); do echo "selkies-pacer-load-line-$i-0123456789 abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ"; done; done' &
  XPID=$!
  # Polled rather than slept: on a loaded host the terminal can take several
  # seconds to map, and a fixed wait that expires leaves the mover below with
  # no window to move.
  for _ in $(seq 1 100); do
    WID=$(xdotool search --classname "pacer-load" | head -1)
    [ -n "$WID" ] && break
    sleep 0.2
  done
fi
if [ -z "$WID" ]; then
  echo "pacer_load_gen: the load window never appeared on ${DISPLAY}" >&2
  exit 1
fi

i=0
while true; do
  x=$(( 300 + ( ( i % 60 ) * 25 ) ))
  y=$(( 200 + ( ( ( i * 7 ) % 60 ) * 15 ) ))
  if [ $x -gt 1800 ]; then x=$(( 3600 - x )); fi
  if [ $y -gt 1100 ]; then y=$(( 2200 - y )); fi
  # The window going away IS the stop signal. Without this the loop outlives
  # the run that started it -- the caller kills the xterm, every move after
  # that fails silently, and the mover spins on a dead id for as long as the
  # machine is up.
  xdotool windowmove "$WID" $x $y 2>/dev/null || exit 0
  i=$(( i + 1 ))
  sleep 0.03
done
