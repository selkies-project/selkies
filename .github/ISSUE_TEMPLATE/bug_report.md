---
name: Bug report
about: Bugs that affect usage
title: ''
labels: bug
assignees: ''

---

**Describe the bug**
A clear and concise description of what the bug is.

**To Reproduce**
Steps to reproduce the behavior:
1. Go to '...'
2. Click on '....'
3. Scroll down to '....'
4. See error or issue

**Expected behavior**
A clear and concise description of what you expected to happen.

**Screenshots**
If applicable, add screenshots to help explain your problem.

 - Host OS Version: [e.g. Ubuntu 24.04, Arch Linux 2024.01.01]
 - Host GPU Model and Driver/Encoder Version: [e.g. Intel UHD Graphics 750, VA-API 1.19, libva 2.12.0 (outputs from `vainfo`) / NVIDIA RTX 3070, 535.129.03 driver]
 - GStreamer Version: [e.g. 1.26.0]
 - Browser Version [e.g. Chrome 121, Safari 17.9]
 - Any other information specific to your environment

**Additional context**
Add any other context about the problem here.

 - [ ] I confirm that this issue is relevant to the scope of this project. If you know that upstream projects are the cause of this problem, please raise the issue there.
 - [ ] I confirm that I have read other open and closed issues and that duplicates do not exist.
 - [ ] I confirm that the issue is easily reproducible and explained thoroughly.
 - [ ] I confirm that relevant log files have been included as explained below. Any relevant additional log files have also been included.
 - [ ] I confirm that no portion of this issue contains credentials or other private information, and it is my own responsibility to protect my privacy.

 - ALL BUGS: upload the output log from `selkies-gstreamer` regardless of whether the bug is caused by the web browser or the host. Read the error. If you are using `docker-*-desktop` container, upload all log files in `/tmp`.
 - If the issue relates to `ximagesrc` and therefore screen capture, upload the Xorg.*.log (such as `/var/log/Xorg.0.log` or `~/.local/share/xorg/Xorg.0.log`).
 - If the issue relates to `webrtcbin` or the web browser, upload contents of `chrome://webrtc-internals` and check in the browser console (F12) to see if there are any errors or warnings. In the browser console, check that the codec is supported in the web browser with `console.log(RTCRtpReceiver.getCapabilities('video').codecs)` after putting in `allow pasting`.
 - Check your TURN server configuration and see if it is valid and connectable. In case the ISP throttles a certain protocol, try turning on TURN over TCP or TURN over TLS.
 - If the issue relates to encoders or `videoconvert`/`cudaconvert`/`vapostproc`, explain your setup and driver installation as precisely as possible.
 - Add any other information as you wish.
