#!/bin/bash
# Fetch the wheels selkies-project releases into the directory given: the
# selkies wheel of the release named (else the newest release; `none` for a
# checkout that installs itself), and the wheels of the newest pixelflux and
# pcmflux releases for this interpreter and machine. A project whose wheel is
# already in the directory is left alone. The GitHub releases are where every
# release is cut, so they are looked in first; a wheel they do not carry is
# reported, for the caller to resolve from the index as its fallback.
# GITHUB_TOKEN, when set, authenticates the lookups.
#
#   fetch-release-wheels.sh <directory> [selkies release tag | latest | none]
set -e

DEST="$1"
RELEASE="${2:-latest}"
ABI="cp$(python3 -c 'import sys; print(f"{sys.version_info.major}{sys.version_info.minor}")')"
AUTH=()
[ -z "${GITHUB_TOKEN:-}" ] || AUTH=(-H "Authorization: Bearer ${GITHUB_TOKEN}")

release_asset() {  # <project> <tag, or latest> <asset name pattern>: the asset's URL
    local api="https://api.github.com/repos/selkies-project/$1/releases"
    # shellcheck disable=SC2016  # $p is jq's, bound by --arg
    local query='.assets[] | select(.name | test($p)) | .browser_download_url'
    if [ "$2" = "latest" ]; then
        api="${api}?per_page=5"
        query=".[] | select(.draft | not) | ${query}"
    else
        api="${api}/tags/$2"
    fi
    curl -fsSL --retry 5 --connect-timeout 30 "${AUTH[@]}" "${api}" \
        | jq -r --arg p "$3" "${query}" | head -n 1 | grep .
}

fetch_wheel() {  # <project> <tag, or latest> <asset name pattern>
    local url
    ls "${DEST}/$1-"*.whl > /dev/null 2>&1 && return 0
    url="$(release_asset "$@")" || return 1
    curl -fsSL --retry 5 --connect-timeout 30 -o "${DEST}/${url##*/}" "${url}"
}

mkdir -p "${DEST}"
[ "${RELEASE}" = "none" ] || fetch_wheel selkies "${RELEASE}" '^selkies-.*-py3-none-any\.whl$' \
    || echo "warning: no selkies wheel in the ${RELEASE} release; the index is the fallback" >&2
for project in pixelflux pcmflux; do
    fetch_wheel "${project}" latest "^${project}-.*-${ABI}-.*manylinux.*$(uname -m)\\.whl$" \
        || echo "warning: no ${project} wheel for ${ABI} $(uname -m) in its newest release; the index is the fallback" >&2
done
