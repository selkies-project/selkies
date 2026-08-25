#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Faithful repo development setup: builds the web client from source and then
# installs the Python package editable with the web client bundled, exactly as
# the CI wheel build does.
set -e

cd "${WORKSPACE_FOLDER:-/workspaces/selkies}"

# The same script the wheel build, the conda recipe and the root Dockerfile run,
# so the bundle under src/selkies/selkies_web is the one every channel ships
./scripts/ci/build-web.sh

# The pinned pixelflux and pcmflux are a coordinated pre-release, published per
# commit rather than to PyPI, so they are fetched before the editable install
# resolves the pin: the release for each project's current main HEAD when that
# one is published, else the newest there is. Best effort -- without gh, or with
# nothing to download, pip reports the unresolvable pin itself.
WHEELS="$(mktemp -d)"
if command -v gh > /dev/null 2>&1; then
  TAG="cp$(python3 -c 'import sys; print(f"{sys.version_info.major}{sys.version_info.minor}")')"
  for project in pixelflux pcmflux; do
    sha="$(gh api "repos/selkies-project/${project}/commits/main" --jq .sha 2>/dev/null || true)"
    # shellcheck disable=SC2086  # the sha and the tag list are single words each
    for candidate in ${sha} $(gh release list --repo "selkies-project/${project}" \
        --limit 5 --json tagName --jq '.[].tagName' 2>/dev/null); do
      if gh release download "${candidate}" --repo "selkies-project/${project}" \
          --pattern "*${TAG}-*manylinux*$(uname -m)*.whl" \
          --dir "${WHEELS}" --skip-existing > /dev/null 2>&1; then
        echo "${project}: ${candidate}"
        break
      fi
    done
  done
fi

PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --user --find-links "${WHEELS}" -e .
rm -rf "${WHEELS}"
echo "Selkies installed editable with the bundled web client. Start it with: start-selkies.sh"
