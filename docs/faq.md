# Troubleshooting and FAQs

## The HTML5 web interface loads and the signaling connection works, but the WebRTC connection fails or the remote desktop does not start.

<details markdown>
  <summary>Open Answer</summary>

First of all, ensure that there is a running PulseAudio or PipeWire-Pulse session as the interface does not establish without an audio server.

**Moreover, check that you are using X.Org instead of Wayland (which is the default in many distributions but not supported) when using an existing display.**

**Then, please read [WebRTC and Firewall Issues](firewall.md).**

Also check if the WebRTC video codec is supported in the web browser, as the server may panic if the codecs do not match. H.264, VP8, and VP9 are supported by all major web browsers.

Moreover, if using HTTP but not HTTPS on a remote host that is not `localhost`, use port forwarding to `localhost` as much as possible. Many browsers do not support WebRTC or relevant features including pointer and keyboard lock in HTTP outside localhost.

If you created the TURN server or the example container inside a VPN-enabled environment or virtual machine and the WebRTC connection fails, then you may need to add the `SELKIES_TURN_HOST` environment variable to the private VPN IP of the TURN server host, such as `192.168.0.2` (IPv4) or `[fe80::2]` (IPv6, including the square brackets).

Make sure to also check that you enabled automatic login with your display manager, as the remote desktop cannot access the initial login screen after boot without login. 

</details>

## The HTML5 web interface is slow, lagging, or stuttering.

<details markdown>
  <summary>Open Answer</summary>

**First, check if the TURN server is shown as `staticauth.openrelay.metered.ca` with a `relay` connection, and if so, please read [WebRTC and Firewall Issues](firewall.md).**

**Usually, if the host-client distance is not too far physically, the issue arises from using a Wi-Fi router with bufferbloat issues, especially if you observe stuttering. Try using the [Bufferbloat Test](https://www.waveform.com/tools/bufferbloat) to identify the issue first before moving on.**

If this is the case, first try enabling `--congestion_control`, meant to mitigate such issues in coordination with the web browser.

Moreover, always make sure that there are minimal background network processes, as live interactive streaming is much less tolerant to network fluctuation compared with other forms of video that may load the stream in advance. Using wired ethernet or a good 5GHz Wi-Fi connection is important (wired ethernet will eliminate all remaining issues of a good but slightly stuttering Wi-Fi connection).

Ensure the latency to your TURN server from the server and the client is ideally under 50-75 ms. If the latency is too high, your connection might be too laggy for most interactive 3D applications.

Next, there currently exists a current issue with CPU congestion from the web interface when the side panel is open. Please make sure to test your experience when the side panel is closed.

Also note that a higher framerate will improve performance if you have sufficient bandwidth. This is because one screen refresh from a 60 fps screen takes 16.67 ms at a time, while one screen refresh from a 15 fps screen inevitably takes 66.67 ms, and therefore inherently causes a visible lag. Also try to keep the total bitrate reasonable, keeping around your service level agreement (SLA) bandwidth (which might be different from your maximum bandwidth contract).

If the latency becomes higher while the screen is idle or the tab is not focused for a long time, the internal efficiency control mechanism of the web browser may activate, which will be resolved automatically after a few seconds if there is new activity.

If it does not, disable all power saving or efficiency features available in the web browser. In Windows 10 or 11, try `Start > Settings > System > Power & battery > Power mode > Best performance`. Also, note that if you saturate your CPU or GPU with an application on the host, the remote desktop interface will also substantially slow down as it cannot use the CPU or GPU enough to decode the screen. Also, check for GPU driver/firmware updates in the client computer.

However, it might be that the parameters for the WebRTC interface, video encoders, the RTP payloader, or other [GStreamer](https://gstreamer.freedesktop.org) plugins are not optimized enough. If you find that it is the case, we always welcome [contributions](development.md). If your changes show noticeably better results in the same conditions, please make a [Pull Request](https://github.com/selkies-project/selkies/pulls), or tell us about the parameters in any channel that we can reach so that we could also test.

</details>

## The clipboard does not work.

<details markdown>
  <summary>Open Answer</summary>

This is very likely a web browser constraint that is applied because you are using HTTP for an address to the web interface that is not localhost. The clipboard only works when you use HTTPS (with a valid or self-signed certificate), or when accessing localhost (some browsers do not support this as well). You could use port forwarding to access through localhost or obtain an HTTPS certificate.

</details>

## The web interface refuses to start up in the terminal after rebooting my computer or restarting my desktop in a standalone instance.

<details markdown>
  <summary>Open Answer</summary>

This is because the desktop session starts as `root` when the user is not logged in. Next time, set up automatic login in the settings with the user you want to use.

In order to use the web interface when this is not possible (or when you are using SSH or other forms of remote access), check `sudo systemctl status sddm`, `sudo systemctl status lightdm`, or `sudo systemctl status gdm3` (use your display session manager) and find the path next to the `-auth` argument. Set the environment variable `XAUTHORITY` to the path you found while running Selkies-GStreamer as `root` or `sudo`.

</details>

## My touchpad does not move while pressing a key with the keyboard.

<details markdown>
  <summary>Open Answer</summary>

This is a setting from the client operating system and will show the same behavior with any other application. In Windows, go to `Settings > Bluetooth & devices > Touchpad > Taps` to increase your touchpad sensitivity. In Linux or Mac, turn off the setting `Touchpad > Disable while typing`.

</details>

## I want to pass multiple screens within a server to another client using the WebRTC HTML5 web interface.

<details markdown>
  <summary>Open Answer</summary>

You can start a new instance of Selkies-GStreamer by changing the `DISPLAY` environment variable (or even use the same one for multiple instances) and setting a different web interface port in a different terminal to pass a different screen simultaneously to your current screen. Reverse proxy server/web servers supporting WebSocket such as `nginx` can be utilized to expose the interfaces to multiple users in different paths.

</details>

## I want to test a shared secret TURN server by manually generating a TURN credential from a shared secret.

<details markdown>
  <summary>Open Answer</summary>

Try the [TURN-REST Container](component.md#turn-rest) or its underlying turn-rest `app.py` Flask web application. This will output TURN credentials automatically when the Docker®/Podman options `-e TURN_SHARED_SECRET=`, `-e TURN_HOST=`, `-e TURN_PORT=`, `-e TURN_PROTOCOL=`, `-e TURN_TLS=` or environment variables `export TURN_SHARED_SECRET=`, `export TURN_HOST=`, `export TURN_PORT=`, `export TURN_PROTOCOL=`, `export TURN_TLS=` are set.

The below steps can be used when you want to test your TURN server configured with a shared secret instead of the legacy username/password authentication:

**1. Run the [Example Container](component.md#example-container) (fill in `DISTRIB_RELEASE` to Ubuntu versions such as `24.04`):**

```bash
docker run --name selkies -it -d --rm -p 8080:8080 -p 3478:3478 ghcr.io/selkies-project/selkies-gstreamer/gst-py-example:main-ubuntu${DISTRIB_RELEASE}
docker exec -it selkies bash
```

Add `--gpus 1 --runtime nvidia` to `docker run` when using NVIDIA GPUs.

**2. From inside the test container, call the `generate_rtc_config` method.**

```bash
export SELKIES_TURN_HOST="YOUR_TURN_HOST"
export SELKIES_TURN_PORT="YOUR_TURN_PORT"
export SELKIES_TURN_SECRET="YOUR_SHARED_SECRET"
export SELKIES_TURN_USER="user"

python3 -c 'import os;from selkies import generate_rtc_config; print(generate_rtc_config(os.environ["SELKIES_TURN_HOST"], os.environ["SELKIES_TURN_PORT"], os.environ["SELKIES_TURN_SECRET"], os.environ["SELKIES_TURN_USER"]))'
```

Using both methods, you can then test your TURN server configuration from the [Trickle ICE](https://webrtc.github.io/samples/src/content/peerconnection/trickle-ice/) website.

</details>
