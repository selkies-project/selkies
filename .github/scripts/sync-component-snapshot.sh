#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Create a fast-forward, full-tree component snapshot from a canonical Selkies commit.
set -euo pipefail

readonly branch=monorepo-import
readonly metadata_path=.monorepo-transition.json
readonly controller_path=.github/workflows/monorepo-transition.yml

usage() {
    cat <<'EOF'
Usage: sync-component-snapshot.sh --component pixelflux|pcmflux \
  --source-dir PATH --source-repository selkies-project/selkies \
  --source-ref monorepo-import|main --target-dir PATH \
  --target-repository selkies-project/PROJECT [--output PATH]

The target must be a clean checkout of its default main branch with
refs/remotes/origin/monorepo-import fetched when that branch exists.
EOF
}

fail() {
    printf 'sync-component-snapshot: %s\n' "$*" >&2
    exit 1
}

component=''
source_dir=''
source_repository=''
source_ref=''
target_dir=''
target_repository=''
output=''

while (($#)); do
    case "$1" in
        --component|--source-dir|--source-repository|--source-ref|--target-dir|--target-repository|--output)
            (($# >= 2)) || fail "$1 requires a value"
            case "$1" in
                --component) component=$2 ;;
                --source-dir) source_dir=$2 ;;
                --source-repository) source_repository=$2 ;;
                --source-ref) source_ref=$2 ;;
                --target-dir) target_dir=$2 ;;
                --target-repository) target_repository=$2 ;;
                --output) output=$2 ;;
            esac
            shift 2
            ;;
        --help|-h)
            (($# == 1)) || fail '--help takes no additional arguments'
            usage
            exit 0
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

[[ -n "$component" && -n "$source_dir" && -n "$source_repository" && -n "$source_ref" && -n "$target_dir" && -n "$target_repository" ]] || {
    usage >&2
    exit 2
}
case "$component" in
    pixelflux|pcmflux) ;;
    *) fail 'component must be pixelflux or pcmflux' ;;
esac
[[ "$source_repository" == selkies-project/selkies ]] || fail 'canonical repository must be selkies-project/selkies'
case "$source_ref" in
    monorepo-import|main) ;;
    *) fail 'canonical ref must be monorepo-import or main' ;;
esac
[[ "$target_repository" == "selkies-project/${component}" ]] || fail "target repository must be selkies-project/${component}"
command -v git >/dev/null || fail 'git is required'
command -v python3 >/dev/null || fail 'python3 is required'

source_dir="$(cd -- "$source_dir" && pwd -P)" || fail 'source directory is inaccessible'
target_dir="$(cd -- "$target_dir" && pwd -P)" || fail 'target directory is inaccessible'
[[ "$(git -C "$source_dir" rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] || fail 'source directory is not a Git worktree'
[[ "$(git -C "$target_dir" rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] || fail 'target directory is not a Git worktree'
[[ "$(git -C "$target_dir" symbolic-ref --quiet --short HEAD)" == main ]] || fail 'target must be checked out on main'
[[ -z "$(git -C "$target_dir" status --porcelain=v1 --untracked-files=all)" ]] || fail 'target worktree must be clean, including untracked files'
git -C "$target_dir" config user.name >/dev/null || fail 'target Git user.name is required'
git -C "$target_dir" config user.email >/dev/null || fail 'target Git user.email is required'

source_commit="$(git -C "$source_dir" rev-parse 'HEAD^{commit}')"
source_tree="$(git -C "$source_dir" rev-parse 'HEAD^{tree}')"
main_commit="$(git -C "$target_dir" rev-parse 'refs/heads/main^{commit}')"
remote_ref="refs/remotes/origin/${branch}"
branch_ref="refs/heads/${branch}"
mirror_commit=''
local_mirror_commit=''
if git -C "$target_dir" rev-parse --verify --quiet "$branch_ref^{commit}" >/dev/null; then
    local_mirror_commit="$(git -C "$target_dir" rev-parse "$branch_ref^{commit}")"
fi
if git -C "$target_dir" rev-parse --verify --quiet "$remote_ref^{commit}" >/dev/null; then
    mirror_commit="$(git -C "$target_dir" rev-parse "$remote_ref^{commit}")"
elif [[ -n "$local_mirror_commit" ]]; then
    fail 'local snapshot branch exists without its fetched remote branch'
fi

# Import the exact canonical ancestry so every snapshot's second parent is the
# canonical commit, retaining its original authors in the public branch graph.
git -C "$target_dir" fetch --quiet --no-tags --no-write-fetch-head -- "$source_dir" "$source_commit"

temporary="$(mktemp -d "${TMPDIR:-/tmp}/component-snapshot.XXXXXXXX")"
trap 'rm -rf -- "$temporary"' EXIT
index="$temporary/index"

install_controller() {
    local index_file=$1
    local commit=$2
    local entry mode blob
    if entry="$(git -C "$target_dir" ls-tree "$commit" -- "$controller_path")" && [[ -n "$entry" ]]; then
        mode="${entry%% *}"
        blob="$(git -C "$target_dir" rev-parse "$commit:$controller_path")"
        case "$mode" in
            100644|100755) ;;
            *) fail 'transition controller must be a regular file' ;;
        esac
        [[ "$(git -C "$target_dir" cat-file -t "$blob")" == blob ]] || fail 'transition controller must be a blob'
        GIT_INDEX_FILE="$index_file" git -C "$target_dir" update-index --add --cacheinfo "${mode},${blob},${controller_path}"
    else
        GIT_INDEX_FILE="$index_file" git -C "$target_dir" update-index --force-remove -- "$controller_path"
    fi
}

previous_tree=''
if [[ -n "$mirror_commit" ]]; then
    git -C "$target_dir" show "$mirror_commit:$metadata_path" > "$temporary/previous.json" || fail 'snapshot has no valid transition metadata'
    python3 - "$temporary/previous.json" "$temporary/previous.values" "$component" "$source_repository" <<'PY'
import json
import re
import sys

source, destination, component, repository = sys.argv[1:]
try:
    with open(source, encoding="utf-8") as stream:
        data = json.load(stream)
except (OSError, UnicodeError, json.JSONDecodeError) as error:
    raise SystemExit(f"invalid transition metadata: {error}")
expected = {"component", "canonical_repository", "canonical_ref", "canonical_commit", "canonical_tree"}
if set(data) != expected:
    raise SystemExit("transition metadata has an unexpected schema")
if data["component"] != component or data["canonical_repository"] != repository:
    raise SystemExit("transition metadata names another component or canonical repository")
if data["canonical_ref"] not in {"monorepo-import", "main"}:
    raise SystemExit("transition metadata names an invalid canonical ref")
if not all(re.fullmatch(r"[0-9a-f]{40}", data[key]) for key in ("canonical_commit", "canonical_tree")):
    raise SystemExit("transition metadata contains an invalid object ID")
with open(destination, "w", encoding="utf-8", newline="\n") as stream:
    stream.write(f'{data["canonical_commit"]}\n{data["canonical_tree"]}\n{data["canonical_ref"]}\n')
PY
    mapfile -t previous < "$temporary/previous.values"
    ((${#previous[@]} == 3)) || fail 'could not read previous transition metadata'
    previous_commit=${previous[0]}
    previous_tree=${previous[1]}
    git -C "$source_dir" cat-file -e "$previous_commit^{commit}" 2>/dev/null || fail 'previous canonical commit is unavailable'
    [[ "$(git -C "$source_dir" rev-parse "$previous_commit^{tree}")" == "$previous_tree" ]] || fail 'previous canonical tree does not match its metadata'
    git -C "$source_dir" merge-base --is-ancestor "$previous_commit" "$source_commit" || fail 'canonical history does not contain the previous snapshot commit'
    git -C "$target_dir" cat-file -e "$previous_commit^{commit}" 2>/dev/null || fail 'previous canonical commit is missing from the target object database'

    rm -f -- "$index"
    GIT_INDEX_FILE="$index" git -C "$target_dir" read-tree "$previous_commit^{tree}"
    previous_metadata_blob="$(git -C "$target_dir" rev-parse "$mirror_commit:$metadata_path")"
    GIT_INDEX_FILE="$index" git -C "$target_dir" update-index --add --cacheinfo "100644,${previous_metadata_blob},${metadata_path}"
    install_controller "$index" "$mirror_commit"
    expected_tree="$(GIT_INDEX_FILE="$index" git -C "$target_dir" write-tree)"
    actual_tree="$(git -C "$target_dir" rev-parse "$mirror_commit^{tree}")"
    [[ "$actual_tree" == "$expected_tree" ]] || fail 'snapshot contains direct edits or extra paths; land changes in canonical Selkies first (nothing updated)'
fi

python3 - "$temporary/metadata.json" "$component" "$source_repository" "$source_ref" "$source_commit" "$source_tree" <<'PY'
import json
import sys

path, component, repository, ref, commit, tree = sys.argv[1:]
data = {
    "canonical_commit": commit,
    "canonical_ref": ref,
    "canonical_repository": repository,
    "canonical_tree": tree,
    "component": component,
}
with open(path, "w", encoding="utf-8", newline="\n") as stream:
    json.dump(data, stream, indent=2, sort_keys=True)
    stream.write("\n")
PY
metadata_blob="$(git -C "$target_dir" hash-object -w -- "$temporary/metadata.json")"
rm -f -- "$index"
GIT_INDEX_FILE="$index" git -C "$target_dir" read-tree "$source_commit^{tree}"
GIT_INDEX_FILE="$index" git -C "$target_dir" update-index --add --cacheinfo "100644,${metadata_blob},${metadata_path}"
install_controller "$index" "$main_commit"
candidate_tree="$(GIT_INDEX_FILE="$index" git -C "$target_dir" write-tree)"

changed=true
created=false
build_required=true
result_commit=$mirror_commit
if [[ -n "$mirror_commit" && "$candidate_tree" == "$(git -C "$target_dir" rev-parse "$mirror_commit^{tree}")" ]]; then
    changed=false
    build_required=false
    if [[ "$local_mirror_commit" != "$mirror_commit" ]]; then
        git -C "$target_dir" update-ref "$branch_ref" "$mirror_commit" "$local_mirror_commit"
    fi
else
    parent=${mirror_commit:-$main_commit}
    [[ -n "$mirror_commit" ]] || created=true
    [[ -z "$previous_tree" || "$source_tree" != "$previous_tree" ]] || build_required=false
    result_commit="$(git -C "$target_dir" commit-tree "$candidate_tree" -p "$parent" -p "$source_commit" -m "Snapshot ${component} from Selkies ${source_commit}")"
    git -C "$target_dir" update-ref "$branch_ref" "$result_commit" "$local_mirror_commit"
fi

{
    printf 'mirror_commit=%s\n' "$result_commit"
    printf 'source_commit=%s\n' "$source_commit"
    printf 'source_tree=%s\n' "$source_tree"
    printf 'changed=%s\n' "$changed"
    printf 'created=%s\n' "$created"
    printf 'build_required=%s\n' "$build_required"
} > "$temporary/outputs"
if [[ -n "$output" ]]; then
    cat "$temporary/outputs" >> "$output"
else
    cat "$temporary/outputs"
fi
