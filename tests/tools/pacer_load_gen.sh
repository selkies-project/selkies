#!/bin/bash
# Animated screen damage for encoder load: a large xterm with scrolling text
# moved in a Lissajous pattern so the encoder sees fresh full-rect damage.
# Driven by tests/perf/test_pacer.py; needs xterm and xdotool.
export DISPLAY="${E2E_DISPLAY:-:99}"
WID=$(xdotool search --name "pacer-load" | head -1)
if [ -z "$WID" ]; then
  xterm -geometry 220x45 -name pacer-load -e bash -c 'while true; do for i in $(seq 1 50); do echo "selkies-pacer-load-line-$i-0123456789 abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ"; done; done' &
  XPID=$!
  sleep 1
  WID=$(xdotool search --name "pacer-load" | head -1)
fi
i=0
while true; do
  x=$(( 300 + ( ( i % 60 ) * 25 ) ))
  y=$(( 200 + ( ( ( i * 7 ) % 60 ) * 15 ) ))
  if [ $x -gt 1800 ]; then x=$(( 3600 - x )); fi
  if [ $y -gt 1100 ]; then y=$(( 2200 - y )); fi
  xdotool windowmove "$WID" $x $y 2>/dev/null
  i=$(( i + 1 ))
  sleep 0.03
done
