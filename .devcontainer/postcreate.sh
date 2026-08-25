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

# gh carries no retry of its own, and every call below is a network call. The
# two discovery calls are the ones worth retrying: the repository and its
# release list always exist, so a failure there is transient by elimination,
# and an empty answer is indistinguishable from "nothing is published" -- a
# blip silently collapses the candidate list, and the editable install then
# fails on an unresolvable pin rather than on anything the developer did.
gh_retry() {
  i=1
  until "$@"; do
    [ "${i}" -ge 5 ] && return 1
    i=$((i + 1)); sleep 3
  done
}

if command -v gh > /dev/null 2>&1; then
  TAG="cp$(python3 -c 'import sys; print(f"{sys.version_info.major}{sys.version_info.minor}")')"
  PATTERN="*${TAG}-*manylinux*$(uname -m)*.whl"
  for project in pixelflux pcmflux; do
    sha="$(gh_retry gh api "repos/selkies-project/${project}/commits/main" --jq .sha 2>/dev/null || true)"
    # shellcheck disable=SC2086  # the sha and the tag list are single words each
    for candidate in ${sha} $(gh_retry gh release list --repo "selkies-project/${project}" \
        --limit 5 --json tagName --jq '.[].tagName' 2>/dev/null); do
      # Not retried, deliberately: a candidate with no matching wheel is the
      # normal case -- the sha usually has no release yet -- and that failure
      # is what selects the next candidate. Retrying it would spend five
      # attempts and four sleeps on each miss to no end.
      if gh release download "${candidate}" --repo "selkies-project/${project}" \
          --pattern "${PATTERN}" \
          --dir "${WHEELS}" --skip-existing > /dev/null 2>&1; then
        echo "${project}: ${candidate}"
        break
      fi
    done
  done
fi

# Say which pins are about to go unmet. Without this the pip step below is the
# first sign that anything went wrong, and it reports the pin rather than the
# fetch that failed to satisfy it.
for project in pixelflux pcmflux; do
  ls "${WHEELS}"/${project}-*.whl > /dev/null 2>&1 ||
    echo "warning: no ${project} wheel fetched; the editable install will resolve its pin from PyPI" >&2
done

PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --retries 5 --timeout 60 \
  --user --find-links "${WHEELS}" -e .
rm -rf "${WHEELS}"
echo "Selkies installed editable with the bundled web client. Start it with: start-selkies.sh"
