#!/usr/bin/env python3
"""Where an image under test runs, and how the tier reaches into it.

A target is a published desktop image brought up by tag or digest, as a
Kubernetes pod or a Docker container (on this host or over ssh), or a session
someone already runs. The tier reaches it two ways: a browser opens its HTTPS
port, and `sh` runs a shell command inside it as the session user, with the
session's display, D-Bus, and runtime directory exported, which is how the
checks act on the desktop and read what the server and the session saw.

    E2E_IMAGE_WHERE   kube (default), docker, ssh:HOST, or url:https://HOST:PORT
    E2E_IMAGE_EXEC    url targets: the command prefix that runs `bash -c` inside
                      the session (`docker exec -i NAME`, `kubectl exec -i POD --`)
    E2E_IMAGE_LOGS    url targets: the command printing the server's log, given --tail=N
                      (`kubectl logs POD`, `docker logs NAME`)
    E2E_IMAGE_REACH   kube targets: `forward` reaches the pod through kubectl port-forward
                      (a runner outside the cluster) rather than at its own address
    E2E_IMAGE_KUBECTL the kubectl command (default `kubectl`), for a runner whose
                      HOME is not the one holding the kubeconfig and its tokens
"""
import json
import os
import re
import shlex
import socket
import ssl
import subprocess
import time
import urllib.request
from typing import Any, Dict, List, Optional

# The desktop's own environment, all of it, as an application started from its
# shell or panel gets it: a client of whichever compositor the session runs (so
# its WAYLAND_DISPLAY is the one apps use), with the LD_PRELOAD interposers that
# hand apps the session's gamepads and webcam, and the toolkit settings a
# browser started without them can die on. The images' defaults stand where no
# such process runs yet.
DESKTOP_PROCS = "plasmashell lxqt-panel xfce4-panel pcmanfm-qt mate-panel gnome-shell kwin_x11 kwin_wayland labwc openbox"
SESSION_ENV = (
    'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-$(id -un)}"; '
    f'for n in {DESKTOP_PROCS}; do p=$(pgrep -u "$(id -u)" -x -o "$n") || continue; '
    'while IFS= read -r -d "" kv; do case "$kv" in PWD=*|OLDPWD=*|SHLVL=*|_=*) ;; *) export "$kv";; esac; '
    'done < /proc/$p/environ; break; done 2>/dev/null; '
    'export DISPLAY="${DISPLAY:-:20}"; '
)
NO_VERIFY = ssl.create_default_context()
NO_VERIFY.check_hostname = False
NO_VERIFY.verify_mode = ssl.CERT_NONE


def run(cmd: List[str], timeout: float = 60, data: Optional[bytes] = None) -> subprocess.CompletedProcess:
    """Run a local command, capturing bytes; a timeout reads as exit 124."""
    try:
        return subprocess.run(cmd, input=data, capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired as e:
        return subprocess.CompletedProcess(cmd, 124, e.stdout or b"", e.stderr or b"")


class Target:
    """A running session: its URL, and a shell inside it."""

    name = ""
    url = ""
    image = ""
    digest = ""
    exec_prefix: List[str] = []

    def sh(self, script: str, timeout: float = 60, data: Optional[bytes] = None) -> subprocess.CompletedProcess:
        """Run `script` with bash inside the session, as its user."""
        return run(self.exec_prefix + ["bash", "-c", SESSION_ENV + script], timeout, data)

    def out(self, script: str, timeout: float = 60) -> str:
        """`sh`'s stdout as text, stripped."""
        return self.sh(script, timeout).stdout.decode(errors="replace").strip()

    def put(self, path: str, data: bytes) -> bool:
        """Write `data` to `path` inside the session."""
        q = shlex.quote(path)
        return self.sh(f"mkdir -p $(dirname {q}) && cat > {q}", 120, data).returncode == 0

    def api(self, path: str, body: Optional[dict] = None, timeout: float = 10) -> Any:
        """GET (or POST `body` as JSON to) one of the server's routes; None on failure."""
        req = urllib.request.Request(self.url + path, method="POST" if body is not None else "GET",
                                     data=json.dumps(body).encode() if body is not None else None,
                                     headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=timeout, context=NO_VERIFY) as r:
                raw = r.read()
            return json.loads(raw) if raw[:1] in (b"{", b"[") else raw.decode(errors="replace")
        except Exception:
            return None

    def wait_ready(self, timeout: float = 900) -> bool:
        """Until /api/status answers and the session's desktop has a window manager."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if isinstance(self.api("/api/status"), dict):
                return True
            time.sleep(3)
        return False

    def selkies_log(self, lines: int = 2000) -> str:
        """The tail of what the image's supervisor logged (the server's own lines among it)."""
        return ""

    def alive(self) -> bool:
        """Whether the server still answers (a preempted pod's does not)."""
        return isinstance(self.api("/api/status"), dict)

    def down(self) -> None:
        pass


class KubePod(Target):
    """The image as a pod, which the browser reaches at its IP from inside the cluster (or through a
    port-forward, see `reach`)."""

    def __init__(self, image: str, name: str, env: Dict[str, str], namespace: Optional[str] = None,
                 labels: Optional[Dict[str, str]] = None, gpu: str = "none", affinity: Any = None,
                 cpu: str = "4", memory: str = "8Gi") -> None:
        self.image, self.name, self.env = image, name, env
        self.ns = namespace or os.environ.get("E2E_IMAGE_NAMESPACE") or "default"
        self.labels = {"app": name, **(labels or {})}
        self.gpu, self.affinity, self.cpu, self.memory = gpu, affinity, cpu, memory
        self.kc = shlex.split(os.environ.get("E2E_IMAGE_KUBECTL", "kubectl")) + ["-n", self.ns]
        self.exec_prefix = self.kc + ["exec", "-i", name, "--"]
        self.forward: Optional[subprocess.Popen] = None

    def manifest(self) -> dict:
        limits = {"cpu": str(int(self.cpu) * 2), "memory": self.memory}
        if self.gpu.startswith("nvidia"):
            limits["nvidia.com/gpu"] = "1"
        elif self.gpu == "amd":
            limits["amd.com/gpu"] = "1"
        requests = dict(limits, cpu=self.cpu)
        spec: Dict[str, Any] = {
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 10,
            **({"priorityClassName": os.environ["E2E_IMAGE_PRIORITY_CLASS"]}
               if os.environ.get("E2E_IMAGE_PRIORITY_CLASS") else {}),
            "containers": [{
                "name": "desktop", "image": self.image, "imagePullPolicy": "IfNotPresent",
                "env": [{"name": k, "value": v} for k, v in self.env.items()],
                "ports": [{"containerPort": 8080}],
                "resources": {"requests": requests, "limits": limits},
                # The apps panel's runner (proot) needs ptrace, as the images' docs say.
                "securityContext": {"capabilities": {"add": ["SYS_PTRACE"]}},
                "volumeMounts": [{"name": "dshm", "mountPath": "/dev/shm"}],
            }],
            "volumes": [{"name": "dshm", "emptyDir": {"medium": "Memory", "sizeLimit": "2Gi"}}],
        }
        affinity = json.loads(json.dumps(self.affinity or {}))
        if getattr(self, "avoid", None):
            # A node already tried and unreachable from here is left out (ANDed into every term).
            node = affinity.setdefault("nodeAffinity", {})
            terms = node.setdefault("requiredDuringSchedulingIgnoredDuringExecution", {}).setdefault(
                "nodeSelectorTerms", [{"matchExpressions": []}])
            for term in terms:
                term.setdefault("matchExpressions", []).append(
                    {"key": "kubernetes.io/hostname", "operator": "NotIn", "values": self.avoid})
        if affinity:
            spec["affinity"] = affinity
        if getattr(self, "groups", None):
            spec["securityContext"] = {"supplementalGroups": self.groups}
        return {"apiVersion": "v1", "kind": "Pod",
                "metadata": {"name": self.name, "labels": self.labels}, "spec": spec}

    def status(self) -> dict:
        p = run(self.kc + ["get", "pod", self.name, "-o", "json"], 30)
        return json.loads(p.stdout) if p.returncode == 0 else {}

    def up(self, timeout: float = 1800, tries: int = 6) -> None:
        """Create the pod and wait for its server; a node this host cannot exec
        into or reach is left out of the next try."""
        self.avoid: List[str] = []
        # A pod left by a run that was killed carries its state; every run starts from the image.
        run(self.kc + ["delete", "pod", self.name, "--ignore-not-found", "--wait=true", "--timeout=180s"], 200)
        for _ in range(tries):
            if self._up(timeout):
                return
            node = self.status().get("spec", {}).get("nodeName")
            run(self.kc + ["delete", "pod", self.name, "--wait=true", "--timeout=120s"], 150)
            if node:
                self.avoid.append(node)
        raise RuntimeError(f"pod {self.name} did not come up on {self.avoid}")

    def _up(self, timeout: float) -> bool:
        p = run(self.kc + ["apply", "-f", "-"], 60, json.dumps(self.manifest()).encode())
        if p.returncode != 0:
            raise RuntimeError(f"kubectl apply: {p.stderr.decode(errors='replace')[-300:]}")
        deadline = time.time() + timeout
        while time.time() < deadline:
            pod = self.status()
            if not pod:
                # Preempted or evicted before it ran: the caller tries again.
                return False
            st = pod.get("status", {})
            if st.get("phase") == "Running" and st.get("podIP"):
                cs = (st.get("containerStatuses") or [{}])[0]
                self.digest = cs.get("imageID", "").rsplit("@", 1)[-1]
                self.node = self.status().get("spec", {}).get("nodeName", "")
                if self.sh("true", timeout=45).returncode != 0:
                    return False
                if self.gpu != "none" and not getattr(self, "groups", None):
                    # A render node the host keeps to a group the session user is not in, which Docker's
                    # --group-add covers: the pod comes up again in that group, or every encoder is software.
                    gids = self.out("stat -c %g /dev/dri/renderD* 2>/dev/null; echo :; id -G").split(":")
                    missing = sorted(set(gids[0].split()) - set(gids[-1].split()))
                    if missing:
                        self.groups = [int(g) for g in missing]
                        run(self.kc + ["delete", "pod", self.name, "--wait=true", "--timeout=120s"], 150)
                        return self._up(max(60, deadline - time.time()))
                self.url = self.reach(st["podIP"])
                return bool(self.url) and self.wait_ready(max(60, min(900, deadline - time.time())))
            if st.get("phase") in ("Failed", "Succeeded"):
                return False
            time.sleep(5)
        return False

    def reach(self, ip: str) -> str:
        """The pod's own address (a refusal is the server still starting); "" when
        the cluster routes nowhere from here to that node, which the caller then
        leaves out. With E2E_IMAGE_REACH=forward, a local port-forward instead,
        for a runner outside the cluster (WebRTC then needs a TURN server)."""
        if os.environ.get("E2E_IMAGE_REACH") != "forward":
            try:
                socket.create_connection((ip, 8080), timeout=5).close()
            except ConnectionRefusedError:
                pass
            except OSError:
                return ""
            return f"https://{ip}:8080"
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        self.forward = subprocess.Popen(self.kc + ["port-forward", f"pod/{self.name}", f"{port}:8080"],
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(3)
        return f"https://127.0.0.1:{port}"

    def selkies_log(self, lines: int = 2000) -> str:
        return run(self.kc + ["logs", self.name, f"--tail={lines}"], 60).stdout.decode(errors="replace")

    def down(self) -> None:
        if self.forward:
            self.forward.terminate()
        run(self.kc + ["delete", "pod", self.name, "--wait=false"], 60)


class DockerContainer(Target):
    """The image as a Docker container here, or on `host` over ssh (published on a free port there)."""

    def __init__(self, image: str, name: str, env: Dict[str, str], host: Optional[str] = None,
                 gpus: Optional[str] = None, port: int = 0) -> None:
        self.image, self.name, self.env, self.gpus = image, name, env, gpus
        self.ssh = ["ssh", "-o", "BatchMode=yes", host] if host else []
        self.host = host.split("@")[-1] if host else "127.0.0.1"
        self.port = port or 18000 + (os.getpid() % 1000)
        self.exec_prefix = self.ssh + ["docker", "exec", "-i", "-u", "1000", name]

    def docker(self, *args: str, timeout: float = 120) -> subprocess.CompletedProcess:
        if self.ssh:
            return run(self.ssh + [" ".join(shlex.quote(a) for a in ("docker",) + args)], timeout)
        return run(["docker", *args], timeout)

    def up(self, timeout: float = 1800) -> None:
        self.docker("rm", "-f", self.name)
        args = ["run", "-d", "--name", self.name, "--shm-size", "2g", "--cap-add", "SYS_PTRACE",
                "-p", f"{self.port}:8080"]
        if self.gpus:
            args += ["--gpus", self.gpus]
        for k, v in self.env.items():
            args += ["-e", f"{k}={v}"]
        p = self.docker(*args, self.image, timeout=timeout)
        if p.returncode != 0:
            raise RuntimeError(f"docker run: {p.stderr.decode(errors='replace')[-300:]}")
        info = self.docker("inspect", "--format", "{{.Image}}", self.name).stdout.decode().strip()
        repo = self.docker("image", "inspect", "--format", "{{join .RepoDigests \" \"}}", info).stdout.decode()
        self.digest = (re.findall(r"sha256:[0-9a-f]{64}", repo) or [info])[0]
        self.url = f"https://{self.host}:{self.port}"
        if not self.wait_ready(timeout):
            raise RuntimeError(f"container {self.name} never answered on {self.url}")

    def selkies_log(self, lines: int = 2000) -> str:
        p = self.docker("logs", f"--tail={lines}", self.name, timeout=60)
        return (p.stdout + p.stderr).decode(errors="replace")

    def down(self) -> None:
        self.docker("rm", "-f", self.name)


class Existing(Target):
    """A session that is already up: its URL and the prefix that execs into it."""

    def __init__(self, url: str, exec_prefix: str) -> None:
        self.url, self.name, self.image = url.rstrip("/"), url, url
        self.exec_prefix = shlex.split(exec_prefix)

    def up(self, timeout: float = 120) -> None:
        if not self.wait_ready(timeout):
            raise RuntimeError(f"{self.url} does not answer /api/status")

    def selkies_log(self, lines: int = 2000) -> str:
        cmd = os.environ.get("E2E_IMAGE_LOGS")
        return run(shlex.split(cmd) + [f"--tail={lines}"], 60).stdout.decode(errors="replace") if cmd else ""


def make(image: str, name: str, env: Dict[str, str], **kube: Any) -> Target:
    """The target E2E_IMAGE_WHERE asks for, not yet up."""
    where = os.environ.get("E2E_IMAGE_WHERE", "kube")
    if where == "docker":
        return DockerContainer(image, name, env, gpus=os.environ.get("E2E_IMAGE_DOCKER_GPUS"))
    if where.startswith("ssh:"):
        return DockerContainer(image, name, env, host=where[4:], gpus=os.environ.get("E2E_IMAGE_DOCKER_GPUS"))
    if where.startswith("url:"):
        return Existing(where[4:], os.environ.get("E2E_IMAGE_EXEC", ""))
    return KubePod(image, name, env, **kube)
