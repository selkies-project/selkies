---
display_name: Selkies
description: A low-latency desktop streamed to the browser, with audio, gamepads, and GPU encoding
icon: https://raw.githubusercontent.com/selkies-project/selkies/main/docs/assets/logo/selkies.svg
tags: [desktop, selkies, webrtc, websocket]
---

# Selkies

Add a desktop app to a workspace that has a desktop installed: the module starts
[Selkies](https://github.com/selkies-project/selkies) with the workspace's own
desktop session, and Coder authenticates and proxies it. Its variables are the
KasmVNC module's, so a template swaps one for the other or offers both.

```tf
module "selkies" {
  count               = data.coder_workspace.me.start_count
  source              = "git::https://github.com/selkies-project/selkies.git//addons/coder"
  agent_id            = coder_agent.main.id
  desktop_environment = "xfce"
}
```

`desktop_environment` names an installed session by file, desktop, or program
name, or a command; empty takes the workspace's default desktop. Where the
workspace lacks `selkies-session` or Xvfb, the module installs the release's
native package (`selkies_version`, else the latest) and the distribution's Xvfb,
which takes `sudo` without a password. `wayland = true` streams Selkies' Wayland
backend instead of an Xvfb, and `port`, `subdomain`, `share`, `order`, and
`group` mean what they mean for any app. The log is `/tmp/selkies-session.log`.
