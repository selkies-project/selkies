#!/usr/bin/env python3
"""The apps panel's catalog when proot-apps installs from a local repository.

A session on `PA_REPO_FOLDER` installs only what that folder holds, so the
panel has to list the folder's catalog rather than the remote one: the
repository's own `metadata/metadata.yml` where it carries one, else the
applications the folder holds, named as the runner takes them on the command
line. Its icons are served from the repository's image directory and from
nowhere else. The folder reaches the server through its own setting or
through the variable proot-apps reads, and either way is one absolute path.

Driven against `selkies.apps_catalog` and `selkies.settings` over a repository
laid out in a temporary directory.
"""
import json
import os
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

import helpers as H  # noqa: E402

from selkies import apps_catalog  # noqa: E402
from selkies.settings import SETTING_DEFINITIONS, AppSettings, apps_repo_folder, settings  # noqa: E402

METADATA = "include:\n  - name: firefox\n    full_name: Firefox\n    description: Browser\n    icon: firefox.svg\n"


def repository(root: str, metadata: bool) -> str:
    """Lay out a local repository with two applications under `root`."""
    folder = os.path.join(root, "apps")
    for image in ("ghcr.io_linuxserver_proot-apps_firefox", "ghcr.io_myorg_proot-apps_tool"):
        os.makedirs(os.path.join(folder, image))
    with open(os.path.join(folder, "SHALAYER"), "w") as fh:
        fh.write("loose file\n")
    if metadata:
        os.makedirs(os.path.join(folder, "metadata", "img"))
        with open(os.path.join(folder, "metadata", "metadata.yml"), "w") as fh:
            fh.write(METADATA)
        with open(os.path.join(folder, "metadata", "img", "firefox.svg"), "w") as fh:
            fh.write("<svg/>")
    return folder


def run() -> "H.Results":
    res = H.Results("apps-local-repo")
    with tempfile.TemporaryDirectory() as root:
        folder = repository(root, metadata=True)
        res.check("a repository's own metadata is the catalog, verbatim",
                  apps_catalog.catalog_text(folder) == METADATA)
        icon = apps_catalog.icon_path(folder, "firefox.svg")
        res.check("an icon is served from the repository's image directory",
                  icon == os.path.join(folder, "metadata", "img", "firefox.svg"), icon)
        res.check("an icon the catalog does not have is none",
                  apps_catalog.icon_path(folder, "missing.svg") is None)
        for name in ("../metadata.yml", "..", "img/../../SHALAYER", ""):
            res.check(f"a name that is not a file name is refused: {name!r}",
                      apps_catalog.icon_path(folder, name) is None)
        os.symlink(os.path.join(folder, "SHALAYER"), os.path.join(folder, "metadata", "img", "out.svg"))
        res.check("a link leading out of the image directory is refused",
                  apps_catalog.icon_path(folder, "out.svg") is None)

    with tempfile.TemporaryDirectory() as root:
        folder = repository(root, metadata=False)
        text = apps_catalog.catalog_text(folder)
        names = [app["name"] for app in json.loads(text or "{}").get("include", [])]
        res.check("without metadata the catalog names what the folder holds, as the runner takes it",
                  names == ["firefox", "ghcr.io/myorg/proot-apps:tool"], names)
        res.check("a folder that is not a repository has no catalog",
                  apps_catalog.catalog_text(os.path.join(root, "absent")) is None
                  and apps_catalog.catalog_text("") is None)

    saved_argv, saved_env = sys.argv, dict(os.environ)
    try:
        sys.argv = ["selkies"]
        os.environ.pop("SELKIES_APPS_REPO_FOLDER", None)
        os.environ["PA_REPO_FOLDER"] = "~/repo/"
        fallback = AppSettings(SETTING_DEFINITIONS)
        res.check("the variable proot-apps reads is the setting's fallback",
                  fallback.apps_repo_folder == "~/repo/", fallback.apps_repo_folder)
        os.environ["SELKIES_APPS_REPO_FOLDER"] = "/mnt/apps"
        own = AppSettings(SETTING_DEFINITIONS)
        res.check("the setting's own variable wins over the fallback",
                  own.apps_repo_folder == "/mnt/apps", own.apps_repo_folder)
    finally:
        sys.argv = saved_argv
        os.environ.clear()
        os.environ.update(saved_env)

    saved = settings.apps_repo_folder
    try:
        settings.apps_repo_folder = " ~/repo/ "
        res.check("the folder is read as one absolute path",
                  apps_repo_folder() == os.path.join(os.path.expanduser("~"), "repo"), apps_repo_folder())
        settings.apps_repo_folder = ""
        res.check("no folder means the remote repository", apps_repo_folder() == "")
    finally:
        settings.apps_repo_folder = saved
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(1 if run().failed() else 0)
