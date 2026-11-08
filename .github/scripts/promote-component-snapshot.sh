#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Open or refresh the exact snapshot branch's reviewed pull request into main.
set -euo pipefail

readonly branch=monorepo-import
readonly metadata_path=.monorepo-transition.json

usage() {
    cat <<'EOF'
Usage: promote-component-snapshot.sh --component pixelflux|pcmflux \
  --source-ref monorepo-import|main --source-commit SHA \
  --mirror-commit SHA --target-dir PATH \
  --target-repository selkies-project/PROJECT --run-id NUMBER \
  [--output PATH] [--dry-run]

The target must be a clean main checkout with the exact snapshot fetched as
refs/remotes/origin/monorepo-import. Production use requires GH_TOKEN and gh.
EOF
}

fail() {
    printf 'promote-component-snapshot: %s\n' "$*" >&2
    exit 1
}

component=''
source_ref=''
source_commit=''
mirror_commit=''
target_dir=''
target_repository=''
run_id=''
output=''
dry_run=false
while (($#)); do
    case "$1" in
        --component|--source-ref|--source-commit|--mirror-commit|--target-dir|--target-repository|--run-id|--output)
            (($# >= 2)) || fail "$1 requires a value"
            case "$1" in
                --component) component=$2 ;;
                --source-ref) source_ref=$2 ;;
                --source-commit) source_commit=$2 ;;
                --mirror-commit) mirror_commit=$2 ;;
                --target-dir) target_dir=$2 ;;
                --target-repository) target_repository=$2 ;;
                --run-id) run_id=$2 ;;
                --output) output=$2 ;;
            esac
            shift 2
            ;;
        --dry-run)
            dry_run=true
            shift
            ;;
        --help|-h)
            (($# == 1)) || fail '--help takes no additional arguments'
            usage
            exit 0
            ;;
        *) fail "unknown argument: $1" ;;
    esac
done

[[ -n "$component" && -n "$source_ref" && -n "$source_commit" && -n "$mirror_commit" && -n "$target_dir" && -n "$target_repository" && -n "$run_id" ]] || {
    usage >&2
    exit 2
}
case "$component" in
    pixelflux|pcmflux) ;;
    *) fail 'component must be pixelflux or pcmflux' ;;
esac
case "$source_ref" in
    monorepo-import|main) ;;
    *) fail 'canonical ref must be monorepo-import or main' ;;
esac
[[ "$target_repository" == "selkies-project/${component}" ]] || fail "target repository must be selkies-project/${component}"
[[ "$source_commit" =~ ^[0-9a-f]{40}$ ]] || fail 'source commit must be an exact SHA-1 commit'
[[ "$mirror_commit" =~ ^[0-9a-f]{40}$ ]] || fail 'mirror commit must be an exact SHA-1 commit'
[[ "$run_id" =~ ^[0-9]+$ ]] || fail 'run ID must contain only digits'
command -v git >/dev/null || fail 'git is required'
command -v python3 >/dev/null || fail 'python3 is required'
target_dir="$(cd -- "$target_dir" && pwd -P)" || fail 'target directory is inaccessible'
[[ "$(git -C "$target_dir" symbolic-ref --quiet --short HEAD)" == main ]] || fail 'target must be checked out on main'
[[ -z "$(git -C "$target_dir" status --porcelain=v1 --untracked-files=all)" ]] || fail 'target worktree must be clean, including untracked files'
mirrored="$(git -C "$target_dir" rev-parse "refs/remotes/origin/${branch}^{commit}")"
[[ "$mirrored" == "$mirror_commit" ]] || fail 'fetched snapshot does not match the built commit'
[[ "$(git -C "$target_dir" rev-parse "$mirror_commit^2")" == "$source_commit" ]] || fail 'snapshot second parent is not the canonical source commit'
git -C "$target_dir" show "$mirror_commit:$metadata_path" > /dev/null || fail 'snapshot metadata is unavailable'
python3 - "$target_dir" "$mirror_commit" "$metadata_path" "$component" "$source_ref" "$source_commit" <<'PY'
import json
import subprocess
import sys

repository, mirror, path, component, source_ref, source_commit = sys.argv[1:]
raw = subprocess.check_output(["git", "-C", repository, "show", f"{mirror}:{path}"])
metadata = json.loads(raw)
expected = {
    "component": component,
    "canonical_repository": "selkies-project/selkies",
    "canonical_ref": source_ref,
    "canonical_commit": source_commit,
}
for key, value in expected.items():
    if metadata.get(key) != value:
        raise SystemExit(f"source metadata mismatch for {key}")
PY

emit() {
    if [[ -n "$output" ]]; then
        printf '%s=%s\n' "$1" "$2" >> "$output"
    else
        printf '%s=%s\n' "$1" "$2"
    fi
}

if [[ "$(git -C "$target_dir" rev-parse 'main^{tree}')" == "$(git -C "$target_dir" rev-parse "$mirror_commit^{tree}")" ]]; then
    git -C "$target_dir" merge-base --is-ancestor "$mirror_commit" main || fail 'main matches the tree without snapshot ancestry; use a merge commit for the reviewed promotion'
    emit action none
    emit state up-to-date
    exit 0
fi

temporary="$(mktemp -d "${TMPDIR:-/tmp}/component-promotion.XXXXXXXX")"
trap 'rm -rf -- "$temporary"' EXIT
run_url="https://github.com/${target_repository}/actions/runs/${run_id}"
release_tag="monorepo-${mirror_commit:0:12}"
release_url="https://github.com/${target_repository}/releases/tag/${release_tag}"
title="Update ${component} from canonical Selkies"
cat > "$temporary/body.md" <<EOF
## Canonical component snapshot

- Source: \`selkies-project/selkies@${source_commit}\` (\`${source_ref}\`).
- Snapshot: \`${mirror_commit}\`.
- [Native wheel run](${run_url}) completed successfully for this exact snapshot.
- [GitHub prerelease](${release_url}) contains only ${component} wheels from that run.

This is the full consolidated tree, including the existing Python/PyO3 bridge.
Merge with a merge commit after the required review and checks so the canonical
second-parent ancestry and its authors remain in main. Automation does not
approve or merge this pull request.
EOF

if "$dry_run"; then
    emit action open-or-update
    emit state changes-present
    exit 0
fi

command -v gh >/dev/null || fail 'gh is required unless --dry-run is used'
[[ -n "${GH_TOKEN:-}" ]] || fail 'GH_TOKEN is required unless --dry-run is used'
export GH_PROMPT_DISABLED=1
listing="$temporary/pulls.json"
gh pr list --repo "$target_repository" --state open --base main --head "$branch" \
    --limit 20 --json number,headRefOid,headRepositoryOwner,isCrossRepository,url > "$listing"
python3 - "$listing" "$temporary/matching.json" "$mirror_commit" "$GITHUB_REPOSITORY_OWNER" <<'PY'
import json
import sys

source, destination, mirror, owner = sys.argv[1:]
with open(source, encoding="utf-8") as stream:
    pulls = json.load(stream)
matching = [pull for pull in pulls
            if pull["headRefOid"] == mirror
            and pull["headRepositoryOwner"]["login"] == owner
            and not pull["isCrossRepository"]]
if len(matching) > 1:
    raise SystemExit("multiple open pull requests name the exact snapshot")
with open(destination, "w", encoding="utf-8") as stream:
    json.dump(matching, stream)
PY
mapfile -t matching < <(python3 - "$temporary/matching.json" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    matching = json.load(stream)
print(len(matching))
if matching:
    print(matching[0]["number"])
PY
)
((${#matching[@]} >= 1)) || fail 'could not interpret the pull request search'
action=create
identifier=''
if [[ "${matching[0]}" == 1 ]]; then
    identifier=${matching[1]}
    action=update
    gh pr edit "$identifier" --repo "$target_repository" --title "$title" --body-file "$temporary/body.md" > /dev/null
elif [[ "${matching[0]}" == 0 ]]; then
    gh pr create --repo "$target_repository" --base main --head "$branch" \
        --title "$title" --body-file "$temporary/body.md" > "$temporary/url"
    identifier="$(cat "$temporary/url")"
else
    fail 'multiple matching pull requests require manual resolution'
fi

verification="$temporary/verified.json"
gh pr view "$identifier" --repo "$target_repository" \
    --json number,baseRefName,headRefOid,headRepositoryOwner,isCrossRepository,state,url > "$verification"
python3 - "$verification" "$mirror_commit" "$GITHUB_REPOSITORY_OWNER" <<'PY'
import json
import sys

path, mirror, owner = sys.argv[1:]
with open(path, encoding="utf-8") as stream:
    pull = json.load(stream)
valid = (pull["baseRefName"] == "main"
         and pull["headRefOid"] == mirror
         and pull["headRepositoryOwner"]["login"] == owner
         and not pull["isCrossRepository"]
         and pull["state"] == "OPEN")
if not valid:
    raise SystemExit("pull request identity changed during promotion")
PY
url="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["url"])' "$verification")"
number="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["number"])' "$verification")"
emit "action" "$action"
emit state open
emit "pull_request_number" "$number"
emit "pull_request_url" "$url"
