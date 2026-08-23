#!/usr/bin/env python3
"""The virtual webcam end to end below the transports: encoded frames pushed into
``pixelflux.VirtualCamera`` must come out of the V4L2 device an application sees
through the interposer — right geometry, right pixel format, right colours —
in MMAP and read() mode, for every device format, letterboxed when the camera
does not match the device, and through ffmpeg where it is installed. Also the
keyframe handshake: an inter-coded stream without a keyframe asks for one; the
interposer's PipeWire frame source against the camera's own node; the sysfs view
v4l2-ctl and statx users rely on; and the end-of-source ENODEV a reader gets.
"""
import io
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import pixelflux
from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ADDON = os.path.join(ROOT, "addons", "v4l2-interposer")
TOOLS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "tools")
INTERPOSER = os.path.join(ADDON, "selkies_v4l2_interposer.so")
PROBE = os.path.join(TOOLS, "v4l2probe")

# Limited-range BT.601 of the solid colours the feeder paints (JPEG is full range
# and the camera compresses it into the range V4L2 consumers assume).
RED = (81, 90, 240)
BLUE = (41, 240, 110)
BLACK = (16, 128, 128)
# Limited-range BT.601 green as the encoders (fed limited-range I420) carry it.
GREEN = (145, 54, 34)


def encode_stream(encoder: str, width: int, height: int, frames: int):
    """Encode `frames` solid-green pictures with PyAV; `[(bytes, is_keyframe)]`, or
    None when the encoder is not built in. H.264 comes out Annex-B (what the
    WebRTC depacketizer and WebCodecs' annexb mode hand the camera)."""
    try:
        import av
        ctx = av.codec.CodecContext.create(encoder, "w")
    except Exception:
        return None
    from fractions import Fraction
    ctx.width, ctx.height = width, height
    ctx.pix_fmt = "yuv420p"
    ctx.time_base = Fraction(1, 30)
    ctx.framerate = Fraction(30, 1)
    ctx.bit_rate = 600000
    ctx.gop_size = 15
    opts = {"tune": "zerolatency", "preset": "veryfast", "x264-params": "annexb=1:repeat-headers=1"} if encoder == "libx264" else {"deadline": "realtime", "cpu-used": "8"}
    ctx.options = opts
    try:
        ctx.open()
    except Exception:
        return None
    frame = av.VideoFrame(width, height, "yuv420p")
    for plane, value in zip(frame.planes, GREEN):
        buf = bytearray(plane.buffer_size)
        for i in range(0, plane.buffer_size, plane.line_size):
            buf[i:i + plane.width] = bytes([value]) * plane.width
        plane.update(bytes(buf))
    out = []
    for i in range(frames):
        frame.pts = i
        for pkt in ctx.encode(frame):
            out.append((bytes(pkt), bool(pkt.is_keyframe)))
    for pkt in ctx.encode(None):
        out.append((bytes(pkt), bool(pkt.is_keyframe)))
    return out or None


def build() -> None:
    subprocess.run(["make", "-C", ADDON], check=True, stdout=subprocess.DEVNULL)
    subprocess.run(["make", "-C", TOOLS, "v4l2probe"], check=True, stdout=subprocess.DEVNULL)


def jpeg(width: int, height: int, rgb: tuple) -> bytes:
    buf = io.BytesIO()
    Image.new("RGB", (width, height), rgb).save(buf, format="JPEG", quality=90)
    return buf.getvalue()


class Feeder:
    """Pushes alternating red/blue MJPEG frames at 30 fps from a thread."""

    def __init__(self, cam, width: int, height: int, colors=((255, 0, 0), (0, 0, 255))):
        self.cam = cam
        self.frames = [jpeg(width, height, c) for c in colors]
        self.stop_flag = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.pushed = 0

    def run(self) -> None:
        while not self.stop_flag.is_set():
            self.cam.push(self.frames[self.pushed % len(self.frames)], pixelflux.VirtualCamera.CODEC_MJPEG, True)
            self.pushed += 1
            time.sleep(1 / 30)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.stop_flag.set()
        self.thread.join(2)


def start_camera(sock_dir: str, width: int, height: int, pixel_format: str = "I420"):
    s = pixelflux.VirtualCameraSettings()
    s.socket_path = os.path.join(sock_dir, "selkies_webcam0.sock")
    s.width = width
    s.height = height
    s.pixel_format = pixel_format
    s.device_path = ""
    cam = pixelflux.VirtualCamera()
    cam.start(s)
    return cam


def probe(sock_dir: str, frames: int = 15, mode: str = "mmap", samples=(), timeout: float = 20,
          source: str = "socket", wait_ms: int = 4000, dump: Optional[str] = None) -> dict:
    """Run the libc probe under the interposer and parse its key=value output.

    ``source`` selects the interposer's frame source: the backend socket in
    ``sock_dir`` or the PipeWire node (``sock_dir`` then points nowhere);
    ``dump`` names a file the last frame's bytes are written to.
    """
    env = dict(os.environ, LD_PRELOAD=INTERPOSER, SELKIES_WEBCAM_SOCKET_PATH=sock_dir, SELKIES_WEBCAM_SOURCE=source)
    cmd = [PROBE] + (["--read"] if mode == "read" else []) + ["--timeout", str(wait_ms)]
    if dump:
        cmd += ["--dump", dump]
    for x, y in samples:
        cmd += ["--sample", f"{x},{y}"]
    cmd += ["/dev/video0", str(frames)]
    try:
        p = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return {"error": "probe timed out", "rc": -1}
    out = {"rc": p.returncode, "samples": {}}
    for line in p.stdout.splitlines():
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        if k == "sample":
            pos, yuv = v.split(":")
            out["samples"][tuple(int(n) for n in pos.split(","))] = tuple(int(n) for n in yuv.split(","))
        else:
            out[k] = v
    return out


def near(got, want, tol=6) -> bool:
    return got is not None and all(abs(g - w) <= tol for g, w in zip(got, want))


def jpeg_centre(path: str):
    """(size, centre RGB) of a dumped JPEG frame, or None when it does not decode."""
    try:
        img = Image.open(path).convert("RGB")
        return img.size, img.getpixel((img.width // 2, img.height // 2))
    except Exception:
        return None


def rgb_near(got, want, tol=8) -> bool:
    return got is not None and all(abs(g - w) <= tol for g, w in zip(got, want))


def red_or_blue(yuv) -> bool:
    return near(yuv, RED) or near(yuv, BLUE)


def main() -> int:
    res = H.Results("webcam-device")
    build()
    sock_dir = tempfile.mkdtemp(prefix="selkies-webcam-", dir="/tmp")
    try:
        # --- I420 device, MMAP and read(), geometry and colours ---------------
        cam = start_camera(sock_dir, 640, 480)
        with Feeder(cam, 640, 480):
            r = probe(sock_dir, 15, samples=[(320, 240), (10, 10), (630, 470)])
            res.check("mmap: 15 frames delivered", r.get("rc") == 0 and r.get("frames") == "15", str(r))
            res.check("mmap: device is 640x480 YU12", (r.get("format"), r.get("width"), r.get("height")) == ("YU12", "640", "480"),
                      f"{r.get('format')} {r.get('width')}x{r.get('height')}")
            res.check("mmap: bytesperline/sizeimage for I420", (r.get("bytesperline"), r.get("sizeimage")) == ("640", str(640 * 480 * 3 // 2)),
                      f"{r.get('bytesperline')} {r.get('sizeimage')}")
            res.check("mmap: one format, one size, 30 fps interval", r.get("nformats") == "1" and r.get("timeperframe") == "1/30",
                      f"{r.get('nformats')} {r.get('timeperframe')}")
            res.check("mmap: frame rate keeps up", float(r.get("fps", "0")) >= 15, r.get("fps"))
            res.check("mmap: centre is limited-range red or blue", red_or_blue(r["samples"].get((320, 240))), str(r["samples"]))
            res.check("mmap: corners carry the same solid colour",
                      red_or_blue(r["samples"].get((10, 10))) and red_or_blue(r["samples"].get((630, 470))), str(r["samples"]))
            r = probe(sock_dir, 10, mode="read", samples=[(320, 240)])
            res.check("read(): 10 frames delivered", r.get("rc") == 0 and r.get("frames") == "10", str(r))
            res.check("read(): centre colour", red_or_blue(r["samples"].get((320, 240))), str(r["samples"]))
            # Two consumers at once: the second opens its own handle on the same ring.
            t = threading.Thread(target=lambda: probe(sock_dir, 20), daemon=True)
            t.start()
            r = probe(sock_dir, 20)
            t.join(15)
            res.check("two concurrent consumers", r.get("rc") == 0 and r.get("frames") == "20", str(r))
            ffmpeg = shutil.which("ffmpeg")
            if ffmpeg:
                env = dict(os.environ, LD_PRELOAD=INTERPOSER, SELKIES_WEBCAM_SOCKET_PATH=sock_dir)
                p = subprocess.run([ffmpeg, "-hide_banner", "-loglevel", "error", "-f", "v4l2", "-i", "/dev/video0",
                                    "-frames:v", "10", "-f", "null", "-"], env=env, capture_output=True, text=True, timeout=40)
                res.check("ffmpeg captures 10 frames", p.returncode == 0, p.stderr.strip()[-200:])
            else:
                res.skip("ffmpeg captures 10 frames", "ffmpeg not installed")
            st = cam.stats()
            res.check("stats count decoded and published frames", st["decoded"] > 0 and st["published"] > 0 and st["errors"] == 0, str(st))
            res.check("stats report the input geometry", (st["input_width"], st["input_height"]) == (640, 480), str(st))
            if st.get("pipewire"):
                pw_cli = shutil.which("pw-cli")
                nodes = subprocess.run([pw_cli, "ls", "Node"], capture_output=True, text=True, timeout=20).stdout if pw_cli else ""
                res.check("PipeWire node is published while the daemon is reachable", "selkies-webcam" in nodes, nodes[-200:])
                # The interposer's PipeWire frame source: the same device, the same
                # frames, taken from the node instead of the backend socket.
                nowhere = os.path.join(sock_dir, "no-socket-here")
                r = probe(nowhere, 15, samples=[(320, 240)], source="pipewire")
                res.check("PipeWire source: mmap delivers 15 frames", r.get("rc") == 0 and r.get("frames") == "15", str(r))
                res.check("PipeWire source: device geometry follows the node",
                          (r.get("format"), r.get("width"), r.get("height")) == ("YU12", "640", "480"), str(r))
                res.check("PipeWire source: frame colour", red_or_blue(r["samples"].get((320, 240))), str(r["samples"]))
                r = probe(nowhere, 10, mode="read", samples=[(320, 240)], source="pipewire")
                res.check("PipeWire source: read() delivers 10 frames", r.get("rc") == 0 and r.get("frames") == "10", str(r))
                r = probe(nowhere, 5, source="auto")
                res.check("auto source falls back to PipeWire without a socket", r.get("rc") == 0 and r.get("frames") == "5", str(r))
            else:
                res.skip("PipeWire node is published", "no PipeWire daemon reachable from the test")
            v4l2_ctl = shutil.which("v4l2-ctl")
            if v4l2_ctl:
                env = dict(os.environ, LD_PRELOAD=INTERPOSER, SELKIES_WEBCAM_SOCKET_PATH=sock_dir)
                p = subprocess.run([v4l2_ctl, "-d", "/dev/video0", "--list-formats-ext"], env=env, capture_output=True, text=True, timeout=20)
                res.check("v4l2-ctl identifies the device through sysfs and lists YU12", p.returncode == 0 and "YU12" in p.stdout,
                          (p.stdout + p.stderr).strip()[-200:])
                p = subprocess.run([v4l2_ctl, "--list-devices"], env=env, capture_output=True, text=True, timeout=20)
                res.check("v4l2-ctl --list-devices shows the camera", "Selkies Virtual Camera" in p.stdout and "/dev/video0" in p.stdout,
                          (p.stdout + p.stderr).strip()[-200:])
            else:
                res.skip("v4l2-ctl identifies the device", "v4l2-ctl not installed")
            env = dict(os.environ, LD_PRELOAD=INTERPOSER, SELKIES_WEBCAM_SOCKET_PATH=sock_dir)
            p = subprocess.run(["ls", "-l", "/dev/video0"], env=env, capture_output=True, text=True, timeout=20)
            res.check("ls sees the character device (statx)", p.returncode == 0 and p.stdout.startswith("crw"), (p.stdout + p.stderr).strip()[-120:])
            p = subprocess.run(["cat", "/sys/class/video4linux/video0/name"], env=env, capture_output=True, text=True, timeout=20)
            res.check("sysfs class entry carries the card name", p.stdout.strip() == "Selkies Virtual Camera", (p.stdout + p.stderr).strip()[-120:])
            # A reader blocked on the device sees the source end as ENODEV
            # rather than waiting or spinning on EAGAIN.
            ended = {}
            t = threading.Thread(target=lambda: ended.update(probe(sock_dir, 1000, wait_ms=8000)), daemon=True)
            t.start()
            time.sleep(2)
            t0 = time.monotonic()
        cam.stop()
        t.join(6)
        res.check("stopped source ends a blocked reader with ENODEV",
                  not t.is_alive() and time.monotonic() - t0 < 5 and "No such device" in ended.get("error", ""), str(ended)[:200])
        res.check("stop removes the socket", not os.path.exists(os.path.join(sock_dir, "selkies_webcam0.sock")))

        # --- NV12 and YUYV devices ---------------------------------------------
        for fmt, fourcc, bpl, size in (("NV12", "NV12", 640, 640 * 480 * 3 // 2), ("YUYV", "YUYV", 1280, 640 * 480 * 2)):
            cam = start_camera(sock_dir, 640, 480, fmt)
            with Feeder(cam, 640, 480):
                r = probe(sock_dir, 10, samples=[(320, 240)])
                res.check(f"{fmt}: format/stride/size", (r.get("format"), r.get("bytesperline"), r.get("sizeimage")) == (fourcc, str(bpl), str(size)),
                          f"{r.get('format')} {r.get('bytesperline')} {r.get('sizeimage')}")
                res.check(f"{fmt}: 10 frames with the right colour", r.get("frames") == "10" and red_or_blue(r["samples"].get((320, 240))),
                          str(r.get("samples")))
            cam.stop()

        # --- MJPEG device: the uplink's JPEG frames pass through, the rest is re-encoded ---
        cam = start_camera(sock_dir, 640, 480, "MJPEG")
        dump = os.path.join(sock_dir, "frame.jpg")
        with Feeder(cam, 640, 480):
            r = probe(sock_dir, 10, dump=dump)
            res.check("MJPEG: device format/stride/size",
                      (r.get("format"), r.get("bytesperline"), r.get("sizeimage")) == ("MJPG", "0", str(640 * 480 * 2)),
                      f"{r.get('format')} {r.get('bytesperline')} {r.get('sizeimage')}")
            res.check("MJPEG: 10 JPEG frames", r.get("frames") == "10" and r.get("first_bytes", "").startswith("ffd8"), str(r)[:160])
            px = jpeg_centre(dump)
            res.check("MJPEG: a frame of the device size passes through as the camera's JPEG",
                      px is not None and px[0] == (640, 480) and (rgb_near(px[1], (255, 0, 0)) or rgb_near(px[1], (0, 0, 255))), str(px))
            st = cam.stats()
            res.check("MJPEG: passed through undecoded", st["passthrough"] > 0 and st["decoded"] == 0, str(st))
            if st.get("pipewire"):
                r = probe(os.path.join(sock_dir, "no-socket-here"), 5, source="pipewire", dump=dump)
                res.check("MJPEG: the PipeWire source serves the video/mjpg node",
                          r.get("rc") == 0 and r.get("format") == "MJPG" and r.get("first_bytes", "").startswith("ffd8"), str(r)[:160])
        # Near-white and near-black pictures: a re-encode that confused full- and
        # limited-range samples would land ~20 levels off.
        with Feeder(cam, 320, 240, colors=((250, 250, 250), (5, 5, 5))):
            r = probe(sock_dir, 10, dump=dump)
            px = jpeg_centre(dump)
            res.check("MJPEG: a camera of another size is re-encoded at the device size, full range",
                      r.get("frames") == "10" and px is not None and px[0] == (640, 480)
                      and (rgb_near(px[1], (250, 250, 250)) or rgb_near(px[1], (5, 5, 5))), str(px))
            res.check("MJPEG: re-encoded frames are decoded first", cam.stats()["decoded"] > 0, str(cam.stats()))
        cam.stop()
        h264 = encode_stream("libx264", 320, 240, 45)
        if h264 is None:
            res.skip("MJPEG: H.264 uplink is re-encoded", "PyAV has no libx264 encoder")
        else:
            cam = start_camera(sock_dir, 320, 240, "MJPEG")
            stop_flag = threading.Event()

            def feed_h264(pkts=h264, cam=cam, stop=stop_flag):
                i = 0
                while not stop.is_set():
                    data, key = pkts[i % len(pkts)]
                    cam.push(data, pixelflux.VirtualCamera.CODEC_H264, key)
                    i += 1
                    time.sleep(1 / 30)

            t = threading.Thread(target=feed_h264, daemon=True)
            t.start()
            r = probe(sock_dir, 10, dump=dump)
            stop_flag.set()
            t.join(2)
            px = jpeg_centre(dump)
            res.check("MJPEG: an H.264 uplink is decoded and re-encoded as JPEG",
                      r.get("frames") == "10" and r.get("format") == "MJPG" and px is not None and px[0] == (320, 240)
                      and px[1][1] > 150 and px[1][0] < 90 and px[1][2] < 90, f"{r.get('format')} {px}")
            cam.stop()

        # --- Letterbox: a 16:9 camera into a 4:3 device ------------------------
        cam = start_camera(sock_dir, 640, 480)
        with Feeder(cam, 640, 360):
            r = probe(sock_dir, 10, samples=[(320, 240), (320, 20), (320, 460), (5, 240)])
            s = r["samples"]
            res.check("letterbox: centre keeps the camera colour", red_or_blue(s.get((320, 240))), str(s))
            res.check("letterbox: top and bottom bars are black", near(s.get((320, 20)), BLACK) and near(s.get((320, 460)), BLACK), str(s))
            res.check("letterbox: left edge is picture (not pillarboxed)", red_or_blue(s.get((5, 240))), str(s))
        cam.stop()

        # --- Portrait camera: pillarboxed ---------------------------------------
        cam = start_camera(sock_dir, 640, 480)
        with Feeder(cam, 240, 320):
            r = probe(sock_dir, 10, samples=[(320, 240), (20, 240), (620, 240)])
            s = r["samples"]
            res.check("pillarbox: centre keeps the camera colour", red_or_blue(s.get((320, 240))), str(s))
            res.check("pillarbox: side bars are black", near(s.get((20, 240)), BLACK) and near(s.get((620, 240)), BLACK), str(s))
        cam.stop()

        # --- Real inter-coded streams: H.264 and VP8 through avcodec ------------
        for codec_name, enc_name, codec_id in (("h264", "libx264", pixelflux.VirtualCamera.CODEC_H264),
                                               ("vp8", "libvpx", pixelflux.VirtualCamera.CODEC_VP8)):
            packets = encode_stream(enc_name, 320, 240, 45)
            if packets is None:
                res.skip(f"{codec_name}: decoded colour", f"PyAV has no {enc_name} encoder")
                continue
            cam = start_camera(sock_dir, 320, 240)
            stop_flag = threading.Event()

            def feed(pkts=packets, cam=cam, stop=stop_flag, codec=codec_id):
                i = 0
                while not stop.is_set():
                    data, key = pkts[i % len(pkts)]
                    cam.push(data, codec, key)
                    i += 1
                    time.sleep(1 / 30)

            t = threading.Thread(target=feed, daemon=True)
            t.start()
            r = probe(sock_dir, 20, samples=[(160, 120), (10, 10)])
            stop_flag.set()
            t.join(2)
            st = cam.stats()
            res.check(f"{codec_name}: 20 frames decoded and delivered", r.get("rc") == 0 and r.get("frames") == "20" and st["decoded"] >= 20,
                      f"frames={r.get('frames')} stats={st}")
            res.check(f"{codec_name}: decoded colour is the encoded green", near(r["samples"].get((160, 120)), GREEN, 10) and near(r["samples"].get((10, 10)), GREEN, 10),
                      str(r["samples"]))
            res.check(f"{codec_name}: no decode errors", st["errors"] == 0, str(st))
            cam.stop()

        # --- Keyframe handshake for inter-coded input ---------------------------
        cam = start_camera(sock_dir, 320, 240)
        p_frame = bytes([0, 0, 0, 1, 0x41, 0x9A, 0x00, 0x10])
        wanted = 0
        for _ in range(10):
            wanted |= cam.push(p_frame, pixelflux.VirtualCamera.CODEC_H264, False)
            time.sleep(0.02)
        res.check("H.264 without a keyframe asks for one", bool(wanted & pixelflux.VirtualCamera.KEYFRAME_WANTED), str(wanted))
        st = cam.stats()
        res.check("non-keyframes are skipped, not decoded", st["skipped"] > 0 and st["decoded"] == 0, str(st))
        try:
            cam.push(b"\x00", 42)
            res.check("unknown codec id is rejected", False)
        except ValueError:
            res.check("unknown codec id is rejected", True)
        cam.stop()
        try:
            cam.push(b"\x00", pixelflux.VirtualCamera.CODEC_MJPEG)
            res.check("push after stop raises", False)
        except RuntimeError:
            res.check("push after stop raises", True)
    finally:
        shutil.rmtree(sock_dir, ignore_errors=True)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
