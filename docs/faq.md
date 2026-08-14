---
title: Troubleshooting and FAQs
description: Fixes for connection, performance, clipboard, gamepad, and display problems.
---

## The HTML5 web interface loads and the signaling connection works, but the WebRTC connection fails or the remote desktop does not start.

<details>
  <summary>Open Answer</summary>

This section applies to the opt-in WebRTC transport (`--mode=webrtc`). The default WebSocket transport streams over a single TCP port and does not use STUN/TURN or UDP hole-punching, so it is unaffected by most of the firewall issues below.

First of all, ensure that there is a running PulseAudio or PipeWire-Pulse session as the interface does not establish without an audio server.

**Moreover, when attaching to an existing display, check that you are using X.Org instead of Wayland (the default in many distributions); an already-running Wayland session cannot be captured. The headless Wayland mode (`--wayland=true` / `SELKIES_WAYLAND=true`) runs its own compositor session and is not affected by this check.**

**Then, if you are using WebRTC mode, please read [WebRTC and Firewall Issues](firewall.md).**

In WebRTC mode, also check that H.264 decoding is available in your web browser; both `--encoder-rtc` choices (`h264enc` and `openh264enc`) produce H.264, which all major web browsers support.

Moreover, if using HTTP but not HTTPS on a remote host that is not `localhost`, use port forwarding to `localhost` as much as possible. Many browsers do not support WebRTC or relevant features including pointer and keyboard lock in HTTP outside localhost.

If you created the TURN server or the example container inside a VPN-enabled environment or virtual machine and the WebRTC connection fails, then you may need to add the `SELKIES_TURN_HOST` environment variable to the private VPN IP of the TURN server host, such as `192.168.0.2` (IPv4) or `[fe80::2]` (IPv6, including the square brackets).

Make sure to also check that you enabled automatic login with your display manager, as the remote desktop cannot access the initial login screen after boot without login. 

</details>

## The HTML5 web interface is slow, lagging, or stuttering.

<details>
  <summary>Open Answer</summary>

**First, check if the TURN server is shown as `staticauth.openrelay.metered.ca` with a `relay` connection, and if so, please read [WebRTC and Firewall Issues](firewall.md).**

**Usually, if the host-client distance is not too far physically, the issue arises from using a Wi-Fi router with bufferbloat issues, especially if you observe stuttering. Try using the [Bufferbloat Test](https://www.waveform.com/tools/bufferbloat) to identify the issue first before moving on.**

If this is the case, first try enabling `--congestion-control`, meant to mitigate such issues in coordination with the web browser.

Moreover, always make sure that there are minimal background network processes, as live interactive streaming is much less tolerant to network fluctuation compared with other forms of video that may load the stream in advance. Using wired ethernet or a good 5GHz Wi-Fi connection is important (wired ethernet will eliminate all remaining issues of a good but slightly stuttering Wi-Fi connection).

Ensure the latency to your TURN server from the server and the client is ideally under 50-75 ms. If the latency is too high, your connection might be too laggy for most interactive 3D applications.

Next, the client compiles statistics for the side panel only while it is open, so keep the panel closed when comparing latency or client CPU usage.

Also note that a higher framerate will improve performance if you have sufficient bandwidth. This is because one screen refresh from a 60 fps screen takes 16.67 ms at a time, while one screen refresh from a 15 fps screen inevitably takes 66.67 ms, and therefore inherently causes a visible lag. Also try to keep the total bitrate reasonable, keeping around your service level agreement (SLA) bandwidth (which might be different from your maximum bandwidth contract).

If the latency becomes higher while the screen is idle or the tab is not focused for a long time, the internal efficiency control mechanism of the web browser may activate, which will be resolved automatically after a few seconds if there is new activity.

If it does not, disable all power saving or efficiency features available in the web browser. In Windows 10 or 11, try `Start > Settings > System > Power & battery > Power mode > Best performance`. Also, note that if you saturate your CPU or GPU with an application on the host, the remote desktop interface will also substantially slow down as it cannot use the CPU or GPU enough to decode the screen. Also, check for GPU driver/firmware updates in the client computer.

A client whose hardware video decoder accepts the stream and then fails on it — a driver-level fault that the browser reports only once decoding has started — is switched to software decoding instead of being reloaded onto a lower-quality encoder. That costs client CPU, so the choice is remembered only for the browser build it was made on and is re-probed after a browser update; clearing the site's browser storage also resets it.

However, it might be that the parameters for the transport, the video encoder (`pixelflux`), or the audio encoder (`pcmflux`) are not optimized enough. If you find that it is the case, we always welcome [contributions](development.md). If your changes show noticeably better results in the same conditions, please make a [Pull Request](https://github.com/selkies-project/selkies/pulls), or tell us about the parameters in any channel that we can reach so that we could also test.

</details>

## The clipboard does not work.

<details>
  <summary>Open Answer</summary>

This is very likely a web browser constraint that is applied because you are using HTTP for an address to the web interface that is not localhost. The clipboard only works when you use HTTPS (with a valid or self-signed certificate), or when accessing localhost (some browsers do not support this as well). You could use port forwarding to access through localhost or obtain an HTTPS certificate.

Copy (`Ctrl/Cmd + C`) and paste (`Ctrl/Cmd + V`) work on Chromium, Firefox, and Safari over a secure context. On browsers that block the asynchronous clipboard API, copy-from-session falls back to a synchronous copy automatically, so no browser configuration is needed.

</details>

## The gamepad shows as connected in Selkies, but Steam or a browser inside the remote desktop does not see it.

<details>
  <summary>Open Answer</summary>

Applications reach a Selkies gamepad in one of two ways, and only one of them is a device the kernel knows about.

Where `/dev/uinput` is writable, Selkies registers a real kernel controller ([Kernel Gamepads](component.md#kernel-gamepads)) that every application enumerates normally. If nothing appears, check that the `uinput` module is loaded, that the account running Selkies can write `/dev/uinput`, and that your desktop user can read the `/dev/input/event*` node it creates — the server log names the node and warns when it is unreadable. Steam picks up the controller as a hot-plug, but a Steam that was already running when the pad first appeared may need a restart.

In a container without `/dev/uinput` the [Joystick Interposer](component.md#joystick-interposer) is used instead. It presents the pad only to applications started with it preloaded, which is why Steam and in-desktop browsers cannot find it there.

Also note that the browser Gamepad API only reports controllers in a [secure context](https://developer.mozilla.org/en-US/docs/Web/API/Gamepad_API), so open Selkies over HTTPS or `localhost` and press a button before the pad appears at all.

</details>

## The web interface refuses to start up in the terminal after rebooting my computer or restarting my desktop in a standalone instance.

<details>
  <summary>Open Answer</summary>

This is because the desktop session starts as `root` when the user is not logged in. Next time, set up automatic login in the settings with the user you want to use.

In order to use the web interface when this is not possible (or when you are using SSH or other forms of remote access), check `sudo systemctl status sddm`, `sudo systemctl status lightdm`, or `sudo systemctl status gdm3` (use your display session manager) and find the path next to the `-auth` argument. Set the environment variable `XAUTHORITY` to the path you found while running Selkies as `root` or `sudo`.

</details>

## The video goes black when the screen locks or blanks on an existing desktop.

<details>
  <summary>Open Answer</summary>

Selkies captures one display, and locking or blanking takes the desktop off it. An X11 display manager runs its greeter on a **separate** X server (LightDM spawns `:1` for it), a Wayland session hands its output to the locker, and DPMS blanks the framebuffer on either. Capture keeps running against a display that has stopped drawing, so the stream stays black until the session is unlocked.

The fix is to stop the captured session from idling or locking in the first place. This block is safe to paste on either backend — each line applies where it is meaningful and is skipped where it is not:

```bash
xset s off -dpms                                                    # X11 server-wide
gsettings set org.gnome.desktop.session idle-delay 0                # GNOME, either backend
gsettings set org.gnome.desktop.screensaver lock-enabled false
```

Other desktops keep the same two switches elsewhere: KDE under *Energy Saving* and *Screen Locking* in System Settings, XFCE under *Power Manager* and *Screensaver*, and sway or labwc simply by not running `swayidle`/`swaylock`.

These apply to the running session only, so also stop the desktop from autostarting a screen locker (`light-locker`, `xscreensaver`, `gnome-screensaver`), or it returns at the next login. With NVIDIA GPUs, DPMS blanking may additionally need `Option "HardDPMS" "False"` under the `Device` or `Screen` section of `/etc/X11/xorg.conf`.

None of this applies to a session Selkies brings up itself. The [Example Container](component.md#example-container) starts its X server with `-s 0 -dpms` and installs no locker, and the headless Wayland backend has no display manager to lock.

</details>

## My touchpad does not move while pressing a key with the keyboard.

<details>
  <summary>Open Answer</summary>

This is a setting from the client operating system and will show the same behavior with any other application. In Windows, go to `Settings > Bluetooth & devices > Touchpad > Taps` to increase your touchpad sensitivity. In Linux or Mac, turn off the setting `Touchpad > Disable while typing`.

</details>

## I want to use multiple screens from one server in the HTML5 web interface.

<details>
  <summary>Open Answer</summary>

Selkies has built-in second-display support on both transports: the **Add Screen** button under the side menu's screen settings opens a companion browser window that joins the session as the second screen (the window carries a `#display2-<position>` URL fragment naming which side of the primary it extends), and closing it removes the screen again. Place each window on one of your physical monitors for a dual-screen remote desktop. The `--second-screen` option (`SELKIES_SECOND_SCREEN`) turns the capability off. The headless Wayland backend creates capture outputs on demand, so it needs no preparation (the [Example Container](component.md#example-container) describes how its nested desktop session follows them); only when capturing an external Wayland compositor must that compositor itself expose a second output.

To stream more screens than that, or separate X11 displays, start one Selkies instance per display by changing the `DISPLAY` environment variable and the web interface port in different terminals. Reverse proxy servers/web servers supporting WebSocket such as `nginx` can expose the instances to multiple users under different paths.

</details>

## I want to test a shared secret TURN server by manually generating a TURN credential from a shared secret.

<details>
  <summary>Open Answer</summary>

Try the [TURN-REST Container](component.md#turn-rest) or its underlying turn-rest `app.py` Flask web application. This will output TURN credentials automatically when the Docker®/Podman options `-e TURN_SHARED_SECRET=`, `-e TURN_HOST=`, `-e TURN_PORT=`, `-e TURN_PROTOCOL=`, `-e TURN_TLS=` or environment variables `export TURN_SHARED_SECRET=`, `export TURN_HOST=`, `export TURN_PORT=`, `export TURN_PROTOCOL=`, `export TURN_TLS=` are set.

The below steps can be used when you want to test your TURN server configured with a shared secret instead of the legacy username/password authentication:

**1. Run the [Example Container](component.md#example-container) (set `DISTRIB_RELEASE` to an image flavor, `ubuntu26.04` or `trixie`):**

```bash
docker run --name selkies -it -d --rm -p 8080:8080 -p 3478:3478 ghcr.io/selkies-project/selkies/example:main-${DISTRIB_RELEASE}
docker exec -it selkies bash
```

Add `--gpus 1 --runtime nvidia` to `docker run` when using NVIDIA GPUs.

**2. From inside the test container, call the `generate_rtc_config` method.**

```bash
export SELKIES_TURN_HOST="YOUR_TURN_HOST"
export SELKIES_TURN_PORT="YOUR_TURN_PORT"
export SELKIES_TURN_SHARED_SECRET="YOUR_SHARED_SECRET"
export SELKIES_TURN_USERNAME="user"

python3 -c 'import os;from selkies.webrtc_utils import generate_rtc_config; print(generate_rtc_config(os.environ["SELKIES_TURN_HOST"], os.environ["SELKIES_TURN_PORT"], os.environ["SELKIES_TURN_SHARED_SECRET"], os.environ["SELKIES_TURN_USERNAME"]))'
```

Using both methods, you can then test your TURN server configuration from the [Trickle ICE](https://webrtc.github.io/samples/src/content/peerconnection/trickle-ice/) website.

</details>
