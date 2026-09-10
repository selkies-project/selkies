#!/usr/bin/env python3
"""A bundled conda prefix's entry points run from wherever the prefix ends up.

conda and pip write the prefix's build path into every script they generate, so
an AppImage -- which mounts at a path chosen at run time -- shipped them naming
an interpreter that is not there: `exec: <build path>/bin/python: not found`,
for AppRun and for a hand-run script out of an extracted tree alike.
infra/appimage/relocate-shebangs.sh rewrites them; the shapes below are the
three the two tools write, and each is proven by running the script after the
prefix has moved, which is the only state that tells a working rewrite from a
plausible one.

Also held here are the two couplings that would leave the AppImage assembling
cleanly and running nothing: the plugin has to invoke the rewrite, and
scripts/ci/appimage.sh has to stage it beside the plugin it copies into the
build directory.
"""
import os
import shutil
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
RELOCATE = os.path.join(REPO, "infra", "appimage", "relocate-shebangs.sh")
PLUGIN = os.path.join(REPO, "infra", "appimage", "linuxdeploy-plugin-conda.sh")
APPIMAGE = os.path.join(REPO, "scripts", "ci", "appimage.sh")

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [appimage-relocation] {label}  {detail}", flush=True)


def prefix(root: str, interpreter: str) -> str:
    """A conda-shaped prefix under `root` whose entry points name it.

    The scripts print their own interpreter, which is what says whether a
    rewritten launcher reached the prefix's python or something else on the
    machine. `python3.12` and `python` both exist because conda and pip name
    different ones.
    """
    path = os.path.join(root, "build", "usr", "conda")
    for directory in ("bin", "condabin"):
        os.makedirs(os.path.join(path, directory))
    for name in ("python3.12", "python"):
        os.symlink(interpreter, os.path.join(path, "bin", name))
    body = 'import sys\nprint(sys.argv[0].rsplit("/", 1)[-1], sys.executable)\n'
    scripts = {
        # pip, and conda for a shebang short enough to survive
        "pip": f"#!{path}/bin/python3.12\n{body}",
        # what conda writes for a noarch package's entry point
        "selkies": f"#!/bin/sh\n'''exec' {path}/bin/python \"$0\" \"$@\"\n' '''\n{body}",
        # the same, as conda's shebang rewrite spells it
        "watchmedo": f"#!/bin/sh\n'''exec' \"{path}/bin/python3.12\" \"$0\" \"$@\" #'''\n{body}",
        # a shebang carrying interpreter flags
        "flagged": f"#!{path}/bin/python3.12 -sE\n{body}",
    }
    for name, text in scripts.items():
        write(os.path.join(path, "bin", name), text)
    write(os.path.join(path, "condabin", "conda"), f"#!{path}/bin/python\n{body}")
    # Left alone: an interpreter that is not this prefix's, and a prefix path
    # in a body rather than a launcher (conda's activate names its root there,
    # and is sourced, where $0 is the calling shell)
    write(os.path.join(path, "bin", "host-tool"), "#!/usr/bin/env python3\n" + body)
    write(os.path.join(path, "bin", "activate"),
          f"#!/bin/sh\n_CONDA_ROOT=\"{path}\"\necho \"${{_CONDA_ROOT}}\"\n")
    return path


def write(path: str, text: str) -> None:
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    os.chmod(path, 0o755)


def relocate(path: str) -> subprocess.CompletedProcess:
    return subprocess.run([RELOCATE, path], capture_output=True, text=True)


def ran(command: list) -> str:
    """`command`'s first line of output, or its failure."""
    done = subprocess.run(command, capture_output=True, text=True, timeout=60)
    if done.returncode != 0:
        return f"exit {done.returncode}: {(done.stderr or done.stdout).strip()[:160]}"
    return done.stdout.strip().splitlines()[0] if done.stdout.strip() else "<no output>"


def main() -> int:
    with tempfile.TemporaryDirectory() as root:
        path = prefix(root, sys.executable)
        untouched = {name: open(os.path.join(path, "bin", name), encoding="utf-8").read()
                     for name in ("host-tool", "activate")}

        before = ran([os.path.join(path, "bin", "selkies")])
        check("an entry point naming the build path runs while the prefix is there",
              before.startswith("selkies "), before)

        done = relocate(path)
        check("the rewrite reports nothing left to name", done.returncode == 0,
              (done.stderr or done.stdout).strip()[:200])

        # The prefix moves rather than the scripts: an AppImage's mount point
        # and an extracted copy are both a whole tree somewhere else.
        moved = os.path.join(root, "mount", "usr", "conda")
        os.makedirs(os.path.dirname(moved))
        shutil.move(path, moved)
        # The AppDir's usr/bin, which is what linuxdeploy is handed
        links = os.path.join(root, "mount", "usr", "bin")
        os.makedirs(links)
        for name in ("pip", "selkies"):
            os.symlink(f"../conda/bin/{name}", os.path.join(links, name))

        for name, interp in (("pip", "python3.12"), ("selkies", "python"),
                             ("watchmedo", "python3.12"), ("flagged", "python3.12")):
            out = ran([os.path.join(moved, "bin", name)])
            check(f"{name} runs from the moved prefix on its own python",
                  out == f"{name} {os.path.join(moved, 'bin', interp)}", out)

        out = ran([os.path.join(moved, "condabin", "conda")])
        check("a condabin script reaches the python one directory over",
              out == f"conda {os.path.join(moved, 'bin', 'python')}", out)

        for name in ("pip", "selkies"):
            out = ran([os.path.join(links, name)])
            check(f"usr/bin/{name} resolves the interpreter through the symlink",
                  out.startswith(f"{name} {os.path.join(moved, 'bin')}"), out)

        for name, text in untouched.items():
            body = open(os.path.join(moved, "bin", name), encoding="utf-8").read()
            check(f"{name} is left as it was", body == text, body[:120])

        again = relocate(moved)
        check("a second rewrite is a no-op", again.returncode == 0,
              (again.stderr or again.stdout).strip()[:200])
        out = ran([os.path.join(moved, "bin", "selkies")])
        check("the entry points still run after it", out.startswith("selkies "), out)

    with tempfile.TemporaryDirectory() as root:
        # An interpreter that takes no shell prologue: reported, and the build
        # stops rather than shipping a script that cannot start
        path = prefix(root, sys.executable)
        write(os.path.join(path, "bin", "foreign"),
              f"#!{path}/bin/perl\nprint \"perl\\n\";\n")
        done = relocate(path)
        check("a shebang the rewrite cannot reach fails it",
              done.returncode != 0 and "foreign" in done.stderr,
              f"exit {done.returncode}: {done.stderr.strip()[:160]}")

    plugin = open(PLUGIN, encoding="utf-8").read()
    check("the conda plugin invokes the rewrite",
          "relocate-shebangs.sh" in plugin, "infra/appimage/linuxdeploy-plugin-conda.sh")
    appimage = open(APPIMAGE, encoding="utf-8").read()
    check("the build stages it beside the plugin",
          'cp infra/appimage/relocate-shebangs.sh "${WORK}/relocate-shebangs.sh"' in appimage,
          "scripts/ci/appimage.sh")

    print(f"[appimage-relocation] {passed}/{passed + failed} passed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
