#!/bin/bash
# Fetch the wheels selkies-project releases into the directory given: the
# selkies wheel of the release named for this machine (else the newest
# release; `none` for a checkout that builds its own server), and, when tags
# are given, the pixelflux and pcmflux wheels of those releases for this
# interpreter and machine, which the suites drive beside the server. A project
# whose wheel is already in the directory is left alone. The GitHub releases
# are where every release is cut, so they are looked in first; a wheel they do
# not carry is reported, for the caller to resolve from the index as its
# fallback. A wheel that does not hash to the SHA-256 its release records fails
# the fetch. GITHUB_TOKEN, when set, authenticates the lookups.
#
#   fetch-release-wheels.sh <directory> [selkies release tag | latest | none] [<pixelflux tag> <pcmflux tag>]
set -e

DEST="$1"
RELEASE="${2:-latest}"
AUTH=()
[ -z "${GITHUB_TOKEN:-}" ] || AUTH=(-H "Authorization: Bearer ${GITHUB_TOKEN}")

release_asset() {  # <project> <tag, or latest> <asset name pattern>: the asset's URL and digest
    local api="https://api.github.com/repos/selkies-project/$1/releases"
    # shellcheck disable=SC2016  # $p is jq's, bound by --arg
    local query='.assets[] | select(.name | test($p)) | "\(.browser_download_url) \(.digest // "")"'
    if [ "$2" = "latest" ]; then
        api="${api}?per_page=5"
        query=".[] | select(.draft | not) | ${query}"
    else
        api="${api}/tags/$2"
    fi
    curl -fsSL --retry 5 --connect-timeout 30 "${AUTH[@]}" "${api}" \
        | jq -r --arg p "$3" "${query}" \
        | python3 "$(dirname "$0")/select-compatible-wheels.py" "$1" | head -n 1 | grep .
}

fetch_wheel() {  # <project> <tag, or latest> <asset name pattern>
    local asset url digest
    ls "${DEST}/$1-"*.whl > /dev/null 2>&1 && return 0
    asset="$(release_asset "$@")" || return 1
    url="${asset% *}" digest="${asset##* }"
    curl -fsSL --retry 5 --connect-timeout 30 -o "${DEST}/${url##*/}" "${url}"
    if [ "sha256:$(sha256sum "${DEST}/${url##*/}" | cut -d' ' -f1)" != "${digest}" ]; then
        rm -f "${DEST}/${url##*/}"
        echo "error: ${url##*/} does not match the SHA-256 its release records (${digest:-none})" >&2
        exit 1
    fi
}

mkdir -p "${DEST}"
# A release from before the server was a binary carries one wheel for every machine
[ "${RELEASE}" = "none" ] || fetch_wheel selkies "${RELEASE}" "^selkies-.*-py3-none-(manylinux_.*_$(uname -m)|any)\\.whl$" \
    || echo "warning: no selkies wheel for $(uname -m) in the ${RELEASE} release; the index is the fallback" >&2
if [ "$#" -ge 4 ]; then
    for project in pixelflux pcmflux; do
        case "${project}" in pixelflux) tag="$3" ;; pcmflux) tag="$4" ;; esac
        fetch_wheel "${project}" "${tag}" "^${project}-.*-.*manylinux.*$(uname -m)\\.whl$" \
            || echo "warning: no compatible ${project} wheel for $(python3 --version) $(uname -m) in its ${tag} release" >&2
    done
fi
