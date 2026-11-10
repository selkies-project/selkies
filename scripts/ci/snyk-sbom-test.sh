#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Test SBOMs with `snyk sbom test`, failing on any finding the code does not
# mitigate. snyk-mitigated.json beside this script names each advisory the code
# mitigates where no release fixes it, with the mitigation: Snyk applies no
# ignore to an SBOM test, so the list is kept here, in review with the code.
#
# Usage: snyk-sbom-test.sh <snyk> <org> <sbom>...
set -eu
SNYK="${1:?usage: snyk-sbom-test.sh <snyk> <org> <sbom>...}"
ORG="${2:?usage: snyk-sbom-test.sh <snyk> <org> <sbom>...}"
shift 2
MITIGATED="$(dirname "$0")/snyk-mitigated.json"
RESULT="$(mktemp)"
SEEN="$(mktemp)"
trap 'rm -f "${RESULT}" "${SEEN}"' EXIT

status=0
incomplete=0
for sbom in "$@"; do
    code=0
    "${SNYK}" sbom test --file="${sbom}" --org="${ORG}" --json > "${RESULT}" || code=$?
    # 0 is no finding and 1 findings; anything else is a test that did not run
    if [ "${code}" -gt 1 ] || ! jq -e 'has("dependencyCount")' "${RESULT}" > /dev/null 2>&1; then
        echo "::error::${sbom}: snyk sbom test did not run (exit status ${code})"
        status=1
        incomplete=1
        continue
    fi
    jq -r --arg sbom "${sbom}" '"\($sbom): \(.dependencyCount) dependencies tested, \(.summary)"' "${RESULT}"
    jq -r --slurpfile m "${MITIGATED}" '(.vulnerabilities // []) | unique_by(.id)[] | .id as $id | select($m[0] | has($id))
        | "::notice::\($id) (\(.packageName | split("|") | last)@\(.version)) is mitigated: \($m[0][$id])"' "${RESULT}"
    jq -r '(.vulnerabilities // [])[].id' "${RESULT}" >> "${SEEN}"
    findings="$(jq -r --slurpfile m "${MITIGATED}" '(.vulnerabilities // []) | unique_by(.id)[]
        | .id as $id | select($m[0] | has($id) | not)
        | "\(.id) [\(.severity)] \(.packageName | split("|") | last)@\(.version): \(.title)"' "${RESULT}")"
    if [ -n "${findings}" ]; then
        echo "::error::${sbom}: findings the code does not mitigate"
        printf '%s\n' "${findings}"
        status=1
    fi
done
# An entry no SBOM reports any more, once every test ran, has gone with its
# advisory or its library
if [ "${incomplete}" = 0 ]; then
    for id in $(jq -r 'keys[]' "${MITIGATED}"); do
        grep -qxF "${id}" "${SEEN}" || echo "::warning::${id} is listed in ${MITIGATED} but no SBOM reports it: remove it"
    done
fi
exit "${status}"
