---
title: Jupyter, Coder, and Open OnDemand
description: Offer a Selkies desktop from a Jupyter server, a Coder workspace, or an Open OnDemand portal, behind the platform's own login and proxy, on whatever desktop the host has installed.
---

Each platform below authenticates its users and proxies HTTP and WebSockets to
one port or socket of a process it starts, which is all Selkies needs: the
default WebSocket transport carries the whole session on that one port, and the
client derives its path prefix from the URL it was loaded from. None of this
involves WebRTC, STUN, or TURN.

## selkies-session

What the platform starts is `selkies-session`, which the `selkies` package puts
on `PATH` and the AppImage runs as its first argument, on a host where nothing
can be installed ([Native Install](native.md)). It brings up what the host lacks and
then Selkies, passing it every argument it does not take itself, so the port,
the socket, the login, and every other [setting](settings.md) are Selkies' own,
from the command line or `SELKIES_*` variables alike:

- **Audio**: the sound server the environment names or runs, else PipeWire, else
  PulseAudio, of the session's own.
- **Display**: on the X11 backend, an Xvfb on a free display that admits only the
  session's own cookie, whatever version the host ships; it renders on the GPU
  where the server offers glamor, and comes up without GLX where its GLX cannot
  load. On the Wayland backend (`SELKIES_WAYLAND=true` or `--wayland`), Selkies'
  own compositor, and the `/tmp/.X11-unix` directory a host's boot would make,
  where the XWayland of a nested KDE session puts its socket.
- **Desktop**: a session the host has installed, found as a display manager
  finds one in the `xsessions` or `wayland-sessions` directories of the XDG data
  directories (`~/.local/share` first, so a user adds one without root).
  `--session` names it by file, desktop, or program name (`xfce`, `kde`, `lxqt`,
  `gnome`, `plasma`, `startxfce4`) or gives a command to run instead. Unset, the
  desktop `XDG_CURRENT_DESKTOP` names comes first, then the host's default: the
  user's last choice in `~/.dmrc`, LightDM's `user-session`, GDM's
  `FallbackSession`, the `x-session-manager` alternative, or
  `/etc/sysconfig/desktop`. Without either, a session named after its own
  desktop goes first (`gnome` before `gnome-xorg` or a kiosk session). A name
  only the other backend's sessions carry stands for their desktop, so a
  default recorded for X11 finds the same desktop's Wayland session. On the
  Wayland backend the session is a compositor or a desktop that starts one,
  nested in Selkies' own.

```bash
selkies-session --session=xfce --enable-basic-auth=false            # http://localhost:8080
SELKIES_WAYLAND=true selkies-session --session=labwc --port=8081 --enable-basic-auth=false
```

Everything runs in a runtime directory of the session's own. SIGTERM, SIGINT, or
SIGHUP stops Selkies first and then everything the session started; started in
the background by a script that exits, the session keeps running. To stream a
display that already exists instead, run `selkies` itself.

## Jupyter

The Jupyter server proxy starts and proxies processes beside a notebook server,
and the `selkies` package registers a desktop with it:

```bash
pip install 'selkies[jupyter]'
```

Every Jupyter server in that environment then shows a **Selkies** item in the
launcher and serves the desktop at `<server>/selkies/`, in a tab of its own
since a notebook panel cannot hold the pointer or the keyboard. The proxy
starts `selkies-session` on a Unix socket in a directory only the user reaches
and authenticates the route with Jupyter's token or login, so Selkies runs
without a login of its own; the session ends with the server, a killed one
included. The same holds under JupyterHub, where each user's server offers
their own desktop. The environment the server starts in chooses the rest:
`XDG_CURRENT_DESKTOP` the desktop, `SELKIES_WAYLAND` the backend.

## Coder

A [module](https://github.com/selkies-project/selkies/tree/main/addons/coder)
adds a desktop app to any workspace that has a desktop installed, with the
variables of Coder's KasmVNC module, so a template swaps one for the other or
offers both:

```tf
module "selkies" {
  count               = data.coder_workspace.me.start_count
  source              = "git::https://github.com/selkies-project/selkies.git//addons/coder"
  agent_id            = coder_agent.main.id
  desktop_environment = "xfce"
}
```

Where the workspace has no `selkies-session` or no Xvfb, the module installs
the release's native package and the distribution's Xvfb, which takes `sudo`
without a password, as the KasmVNC module does; `selkies_version` picks the
release. Selkies listens on the workspace's loopback addresses without a login
or TLS of its own, and Coder authenticates and proxies the app, on a subdomain or a
path (`subdomain = false`). `wayland = true` streams the Wayland backend.
`coder port-forward <workspace> --tcp 8080:8080` reaches the same session at
`http://localhost:8080`, which browsers treat as a secure context for the
clipboard, gamepads, and the microphone.

## Open OnDemand

A [batch connect app](https://github.com/selkies-project/selkies/tree/main/addons/ondemand)
on the basic template runs `selkies-session` on the compute node, on the port
the portal chose, and connects the browser through the portal's reverse proxy
at `/rnode/<host>/<port>/`, behind the portal's login:

```bash
sudo cp -r addons/ondemand /var/www/ood/apps/sys/selkies
```

Selkies listens on the node's addresses for the proxy, in
[secure mode](secure-mode.md): the job sets a master token, provisions one
session token once Selkies answers, and only then reports the session running,
and the view's link carries that token, so no one else on the cluster network
reaches the session. `form.yml` offers the desktop and the backend, and holds
in `selkies_command` how the node finds Selkies: `selkies-session` from a native
package or a module, `<AppImage> selkies-session`, or
`apptainer exec --nv <image> selkies-session` with the flags
[Getting Started](start.md#apptainer) gives an image, a home of its own
among them, so the image's Python does not import the user's packages from the
host's home.
`submit.yml.erb` takes what the site's scheduler needs beyond the hours and the
queue, and the portal's `host_regex` has to admit the nodes, as for every
interactive app.
