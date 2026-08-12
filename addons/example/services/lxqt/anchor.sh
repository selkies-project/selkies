#!/bin/bash
# Keeper for the X11 LXQt session on the nested sway compositor, which manages
# the panel as an ordinary floating window and re-centers it on every
# resolution change. One settle pass per sway window/output event burst (plus a
# timed fallback): re-anchor the panel to the bottom edge of its output, and
# keep the desktop tiled so nothing can raise it over the panel. The desktop
# tiles on the primary output alone; a second output shows the compositor
# background, matched to the desktop colour in the sway config so the two read
# as one surface.

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-ubuntu}"

find_swaysock() {
  local pid sock
  for pid in $(pgrep -x sway 2>/dev/null); do
    sock="${XDG_RUNTIME_DIR}/sway-ipc.$(id -u).${pid}.sock"
    [ -S "${sock}" ] && { printf '%s' "${sock}"; return 0; }
  done
  return 1
}

panel_window() {
  wmctrl -l -x 2>/dev/null |
    awk '$3 ~ /^lxqt-panel/ && / LXQt Panel$/ { print $1; exit }'
}

win_geometry() {
  xwininfo -id "$1" 2>/dev/null | awk '
    /Absolute upper-left X/ {x=$4} /Absolute upper-left Y/ {y=$4}
    /^  Width/ {w=$2} /^  Height/ {h=$2} END {if (h != "") print x, y, w, h}'
}

# The output rectangle ("x y w h") holding a point, or the first output when the
# point lies outside every one; the layout comes from sway, so it is right for
# any multi-output arrangement.
output_rect_for_point() {
  local px="$1" py="$2" first="" _n x y w h
  while read -r _n x y w h; do
    [ -n "${first}" ] || first="${x} ${y} ${w} ${h}"
    if [ "${px}" -ge "${x}" ] && [ "${px}" -lt "$((x + w))" ] &&
       [ "${py}" -ge "${y}" ] && [ "${py}" -lt "$((y + h))" ]; then
      printf '%s %s %s %s' "${x}" "${y}" "${w}" "${h}"
      return
    fi
  done < <(swaymsg -t get_outputs 2>/dev/null |
           jq -r '.[] | "\(.name) \(.rect.x) \(.rect.y) \(.rect.width) \(.rect.height)"')
  printf '%s' "${first}"
}

panel_basis=""
panel_ask=""
anchor_panel() {
  local id x y w h ox oy oh strut top bottom target basis
  id="$(panel_window)"
  [ -n "${id}" ] || { panel_basis=""; return; }
  # shellcheck disable=SC2046  # the geometry fields are deliberately split
  set -- $(win_geometry "${id}")
  [ -n "${4:-}" ] || return
  x="$1"; y="$2"; w="$3"; h="$4"
  # shellcheck disable=SC2046
  set -- $(output_rect_for_point $((x + w / 2)) $((y + h / 2)))
  [ -n "${4:-}" ] || return
  ox="$1"; oy="$2"; oh="$4"
  # Strut order is left, right, top, bottom; only an explicit top strut means
  # top — a failed read must not fling the packaged bottom panel upward.
  strut=$(xprop -id "${id}" _NET_WM_STRUT_PARTIAL 2>/dev/null |
          sed -n 's/.*= *//p' | tr -d ' ')
  top=$(printf '%s' "${strut}" | cut -d, -f3)
  bottom=$(printf '%s' "${strut}" | cut -d, -f4)
  if [ "${top:-0}" -gt 0 ] 2>/dev/null && [ "${bottom:-0}" -eq 0 ] 2>/dev/null; then
    target=${oy}
  else
    target=$((oy + oh - h))
  fi
  if [ "${x}" = "${ox}" ] && [ "${y}" = "${target}" ]; then
    panel_basis=""; panel_ask=""
    return
  fi
  basis="${id}:${oy}:${oh}:${h}:${target}"
  if [ "${panel_basis}" = "${basis}" ] && [ -n "${panel_ask}" ]; then
    panel_ask=$((panel_ask + target - y))
  else
    panel_basis="${basis}"
    panel_ask="${target}"
  fi
  # The correction absorbs a frame offset of a few pixels; a larger drift means
  # the measurement raced a resize and the plain target is the answer.
  if [ "${panel_ask}" -gt $((target + 100)) ] || [ "${panel_ask}" -lt $((target - 100)) ]; then
    panel_ask="${target}"
  fi
  swaymsg "[class=\"lxqt-panel\" title=\"^LXQt Panel$\"] border none" >/dev/null 2>&1
  swaymsg "[class=\"lxqt-panel\" title=\"^LXQt Panel$\"] move position ${ox} ${panel_ask}" >/dev/null 2>&1
}

# Keep the desktop a tiling container. Tiled, it is the workspace's only tiling
# element, so sway holds it below every floating window and a click cannot raise
# it over the panel. It tiles on the primary output; other outputs show the
# compositor background, matched to the desktop colour in the sway config.
#
# The desktop is the pcmanfm-qt window of X11 type _NET_WM_WINDOW_TYPE_DESKTOP.
# Its title tells versions apart badly (newer pcmanfm-qt names it
# pcmanfm-desktop-N, older reuses the class), and sway's window_type criteria
# is newer than the oldest sway here, so the window is found by type through
# xprop and addressed by the con_id that type resolves to.
settle_desktop() {
  local xid con
  for xid in $(wmctrl -l -x 2>/dev/null | awk '$3 ~ /pcmanfm-qt/ { print $1 }'); do
    xprop -id "${xid}" _NET_WM_WINDOW_TYPE 2>/dev/null |
      grep -q _NET_WM_WINDOW_TYPE_DESKTOP || continue
    con=$(swaymsg -t get_tree 2>/dev/null | jq -r --argjson w "$((xid))" '
      first(recurse(.nodes[]?, .floating_nodes[]?)
            | select(.window? == $w and .type == "floating_con") | .id) // empty')
    [ -n "${con}" ] &&
      swaymsg "[con_id=${con}] floating disable, border none" >/dev/null 2>&1
  done
}

settle() {
  anchor_panel
  settle_desktop
}

while :; do
  SWAYSOCK="$(find_swaysock)" || { sleep 3; continue; }
  export SWAYSOCK
  settle
  while :; do
    if IFS= read -r -t 3 _event; then
      # A burst settles once, after it goes quiet.
      while IFS= read -r -t 0.2 _event; do :; done
      settle
    else
      rc=$?
      [ "${rc}" -gt 128 ] || break
      settle
    fi
  done < <(swaymsg -t subscribe -m '["window", "output"]' 2>/dev/null)
  sleep 1
done
