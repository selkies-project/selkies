# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""The apps panel's catalog when proot-apps reads a local repository.

proot-apps takes `PA_REPO_FOLDER` as a folder of application tarballs in
place of its remote registry: one directory per application, named for the
image with the separators replaced (`ghcr.io_linuxserver_proot-apps_firefox`),
and optionally a `metadata/` directory holding the catalog its graphical
installer shows (`metadata.yml` and the icons under `img/`). A session on such
a repository installs only what the folder holds, so a panel listing the
remote catalog would offer applications every install of which fails. The
server therefore serves the folder's own catalog over `/api/apps/metadata.yml`
and `/api/apps/img/<name>`, behind the session's own authentication, and
tells the page to read it there (`apps_local_repo` in the settings payload).

A repository without `metadata/` is still a repository: its catalog is written
here from the application directories, the listing proot-apps' own shell
completion offers, each named as the runner takes it on the command line (the
short name for the default namespace, the image reference otherwise) and
nothing more, since nothing more about them is known. The document is JSON,
which is YAML to the page's parser.
"""

import json
import os
from typing import List, Optional

METADATA_DIR = "metadata"
METADATA_FILE = "metadata.yml"
IMAGE_DIR = "img"
# The image folders proot-apps names by their short name alone.
DEFAULT_IMAGE_PREFIX = "ghcr.io_linuxserver_proot-apps_"


def metadata_file(folder: str) -> str:
    """The catalog file a local repository may carry."""
    return os.path.join(folder, METADATA_DIR, METADATA_FILE)


def app_name(image_folder: str) -> str:
    """The command-line name of an application directory, as proot-apps resolves it.

    A directory under the default namespace is the short application name;
    any other is the image reference, `registry/namespace/image:tag`.
    """
    if image_folder.startswith(DEFAULT_IMAGE_PREFIX):
        return image_folder[len(DEFAULT_IMAGE_PREFIX):]
    head, sep, tag = image_folder.rpartition("_")
    if not sep:
        return image_folder
    return f"{head.replace('_', '/')}:{tag}"


def listed_apps(folder: str) -> List[str]:
    """The applications a local repository holds, by command-line name.

    Loose files and the metadata directory are not applications.
    """
    try:
        entries = sorted(os.listdir(folder))
    except OSError:
        return []
    return [app_name(entry) for entry in entries
            if entry != METADATA_DIR and os.path.isdir(os.path.join(folder, entry))]


def catalog_text(folder: str) -> Optional[str]:
    """The catalog document of a local repository, or None where there is no repository.

    The repository's own `metadata/metadata.yml` verbatim when it has one, else
    a document naming the applications the folder holds.
    """
    if not folder or not os.path.isdir(folder):
        return None
    path = metadata_file(folder)
    try:
        with open(path, "r", encoding="utf-8") as fh:
            return fh.read()
    except OSError:
        pass
    include = [{"name": name, "full_name": name, "description": "", "icon": ""}
               for name in listed_apps(folder)]
    return json.dumps({"include": include}, indent=2) + "\n"


def icon_path(folder: str, name: str) -> Optional[str]:
    """The file behind one catalog icon, or None where there is none to serve.

    `name` is a file name the catalog gave, so a value that is not one -- a
    path, a traversal, a NUL byte, a name resolving outside the image
    directory through a link -- names nothing.
    """
    if not folder or not name or name in (".", "..") or "/" in name or os.sep in name or "\0" in name:
        return None
    images = os.path.realpath(os.path.join(folder, METADATA_DIR, IMAGE_DIR))
    path = os.path.realpath(os.path.join(images, name))
    if not path.startswith(images + os.sep):
        return None
    if os.path.dirname(path) != images or not os.path.isfile(path):
        return None
    return path
