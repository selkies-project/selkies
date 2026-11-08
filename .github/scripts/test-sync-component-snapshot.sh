#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Exercise real snapshot branches, including failure paths, in temporary repositories.
set -euo pipefail

scripts="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly helper="$scripts/sync-component-snapshot.sh"
readonly promoter="$scripts/promote-component-snapshot.sh"
readonly evidence=${1:?usage: test-sync-component-snapshot.sh EVIDENCE_DIRECTORY}
mkdir -p -- "$evidence"
work="$(mktemp -d "${evidence%/}/fixture.XXXXXXXX")"
trap 'rm -rf -- "$work"' EXIT
readonly work
readonly repositories="$work/repositories with spaces [fixtures]"
mkdir -p -- "$repositories"
output="$work/output.properties"

fail() {
    printf 'fixture failure: %s\n' "$*" >&2
    exit 1
}

assert_equal() {
    local expected=$1 actual=$2 message=$3
    [[ "$actual" == "$expected" ]] || fail "${message}: expected ${expected}, got ${actual}"
}

output_value() {
    local wanted=$1 name value
    while IFS='=' read -r name value; do
        if [[ "$name" == "$wanted" ]]; then
            printf '%s\n' "$value"
            return 0
        fi
    done < "$output"
    return 1
}

expect_failure() {
    if "$@" > "$work/expected-failure.log" 2>&1; then
        fail "command unexpectedly succeeded: $*"
    fi
}

commit_all() {
    local repository=$1 message=$2
    git -C "$repository" add -A
    git -C "$repository" commit -q -m "$message"
}

run_helper() {
    local source_ref=${1:-monorepo-import}
    : > "$output"
    "$helper" \
        --component pcmflux \
        --source-dir "$source" \
        --source-repository selkies-project/selkies \
        --source-ref "$source_ref" \
        --target-dir "$target" \
        --target-repository selkies-project/pcmflux \
        --output "$output"
}

sync_and_push() {
    run_helper "${1:-monorepo-import}"
    if [[ "$(output_value changed)" == true ]]; then
        git -C "$target" push -q origin refs/heads/monorepo-import:refs/heads/monorepo-import
    fi
    printf '%s\n' "$(output_value mirror_commit)"
}

fetch_mirror() {
    git -C "$target" fetch -q --no-tags origin refs/heads/monorepo-import:refs/remotes/origin/monorepo-import
}

run_promoter() {
    local source_ref=$1 mirror_commit=$2 source_commit=$3
    : > "$output"
    "$promoter" \
        --component pcmflux \
        --source-ref "$source_ref" \
        --source-commit "$source_commit" \
        --mirror-commit "$mirror_commit" \
        --target-dir "$target" \
        --target-repository selkies-project/pcmflux \
        --run-id 12345 \
        --dry-run \
        --output "$output"
}

assert_snapshot() {
    local source_commit=$1 mirror_commit=$2 source_ref=${3:-monorepo-import} actual
    actual="$(git -C "$target" rev-parse "$mirror_commit^{tree}")"
    GIT_INDEX_FILE="$work/verify.index" git -C "$target" read-tree "$mirror_commit^{tree}"
    GIT_INDEX_FILE="$work/verify.index" git -C "$target" update-index --force-remove -- .monorepo-transition.json .github/workflows/monorepo-transition.yml
    actual="$(GIT_INDEX_FILE="$work/verify.index" git -C "$target" write-tree)"
    rm -f -- "$work/verify.index"
    assert_equal "$(git -C "$source" rev-parse "$source_commit^{tree}")" "$actual" 'snapshot differs from full canonical source'
    git -C "$target" merge-base --is-ancestor "$source_commit" "$mirror_commit" || fail 'canonical source is outside snapshot ancestry'
    python3 - "$target" "$mirror_commit" "$source_commit" "$source_ref" <<'PY'
import json
import subprocess
import sys

repository, mirror, source, source_ref = sys.argv[1:]
raw = subprocess.check_output(["git", "-C", repository, "show", f"{mirror}:.monorepo-transition.json"])
metadata = json.loads(raw)
assert metadata["canonical_repository"] == "selkies-project/selkies"
assert metadata["canonical_ref"] == source_ref
assert metadata["canonical_commit"] == source
assert metadata["component"] == "pcmflux"
PY
}

git init -q --bare --initial-branch=monorepo-import "$repositories/canonical.remote"
git init -q --bare --initial-branch=main "$repositories/component.remote"
git init -q -b monorepo-import "$repositories/canonical seed"
git -C "$repositories/canonical seed" config user.name 'Canonical Owner'
git -C "$repositories/canonical seed" config user.email 'canonical@example.invalid'
mkdir -p -- "$repositories/canonical seed/crates/pcmflux/src" "$repositories/canonical seed/crates/pcmflux/scripts" "$repositories/canonical seed/.github/workflows"
printf '[workspace]\nmembers = ["crates/pcmflux"]\n' > "$repositories/canonical seed/Cargo.toml"
printf 'canonical readme\n' > "$repositories/canonical seed/README.md"
printf 'workspace bridge\n' > "$repositories/canonical seed/workspace.py"
printf '[package]\nname = "pcmflux"\nversion = "2.2.1"\n' > "$repositories/canonical seed/crates/pcmflux/Cargo.toml"
printf 'pub fn optional_python_bridge() {}\n' > "$repositories/canonical seed/crates/pcmflux/src/python.rs"
printf '# wheel planner\n' > "$repositories/canonical seed/crates/pcmflux/scripts/wheel-plan.py"
printf 'odd pathname\n' > "$repositories/canonical seed/crates/pcmflux/odd path [source].txt"
ln -s src "$repositories/canonical seed/crates/pcmflux/current"
commit_all "$repositories/canonical seed" 'Initial canonical snapshot'
git -C "$repositories/canonical seed" remote add origin "$repositories/canonical.remote"
git -C "$repositories/canonical seed" push -q -u origin monorepo-import

git init -q -b main "$repositories/component seed"
git -C "$repositories/component seed" config user.name 'Legacy Owner'
git -C "$repositories/component seed" config user.email 'legacy@example.invalid'
mkdir -p -- "$repositories/component seed/.github/workflows"
printf 'legacy readme\n' > "$repositories/component seed/README.md"
printf 'legacy crate\n' > "$repositories/component seed/pcmflux.rs"
printf 'name: Monorepo transition\n' > "$repositories/component seed/.github/workflows/monorepo-transition.yml"
commit_all "$repositories/component seed" 'Legacy default branch'
git -C "$repositories/component seed" remote add origin "$repositories/component.remote"
git -C "$repositories/component seed" push -q -u origin main

git clone -q --branch monorepo-import "$repositories/canonical.remote" "$repositories/canonical clone with spaces"
git clone -q --branch main "$repositories/component.remote" "$repositories/component clone with spaces"
source="$repositories/canonical clone with spaces"
target="$repositories/component clone with spaces"
git -C "$target" config user.name 'Fixture Bot'
git -C "$target" config user.email 'fixture@example.invalid'
legacy_main="$(git -C "$target" rev-parse main)"
source_one="$(git -C "$source" rev-parse HEAD)"

"$helper" --help >/dev/null
expect_failure "$helper" --help unexpected
expect_failure "$helper" --component '../pcmflux' --source-dir "$source" --source-repository selkies-project/selkies --source-ref monorepo-import --target-dir "$target" --target-repository selkies-project/pcmflux
expect_failure "$helper" --component pcmflux --source-dir "$source" --source-repository example.com/selkies --source-ref monorepo-import --target-dir "$target" --target-repository selkies-project/pcmflux
expect_failure "$helper" --component pcmflux --source-dir "$source" --source-repository selkies-project/selkies --source-ref 'monorepo-import;false' --target-dir "$target" --target-repository selkies-project/pcmflux
expect_failure "$helper" --component pcmflux --source-dir "$source" --source-repository selkies-project/selkies --source-ref monorepo-import --target-dir "$target" --target-repository example.com/pcmflux
expect_failure "$helper" --component pcmflux --source-dir "$work/missing" --source-repository selkies-project/selkies --source-ref monorepo-import --target-dir "$target" --target-repository selkies-project/pcmflux

mirror_one="$(sync_and_push)"
assert_equal true "$(output_value created)" 'initial branch creation'
assert_equal true "$(output_value build_required)" 'initial build requirement'
assert_snapshot "$source_one" "$mirror_one"
assert_equal "$legacy_main" "$(git -C "$target" rev-parse "$mirror_one^1")" 'legacy first parent'
assert_equal "$source_one" "$(git -C "$target" rev-parse "$mirror_one^2")" 'initial canonical second parent'
assert_equal 'canonical readme' "$(git -C "$target" show "$mirror_one:README.md")" 'canonical root README'
assert_equal 'pub fn optional_python_bridge() {}' "$(git -C "$target" show "$mirror_one:crates/pcmflux/src/python.rs")" 'Python bridge content'
assert_equal '120000' "$(git -C "$target" ls-tree "$mirror_one" -- crates/pcmflux/current | cut -d' ' -f1)" 'symlink mode'
if git -C "$target" cat-file -e "$mirror_one:crates/pcmflux/crates/pcmflux/Cargo.toml" 2>/dev/null; then
    fail 'snapshot has a doubled crates path'
fi

mirror_repeat="$(sync_and_push)"
assert_equal "$mirror_one" "$mirror_repeat" 'no-op snapshot commit'
assert_equal false "$(output_value changed)" 'no-op change output'
assert_equal false "$(output_value build_required)" 'no-op build output'

printf '// owner improvement\n' >> "$repositories/canonical seed/crates/pcmflux/src/python.rs"
git -C "$repositories/canonical seed" config user.name 'Incoming Owner'
git -C "$repositories/canonical seed" config user.email 'incoming@example.invalid'
commit_all "$repositories/canonical seed" 'Owner source improvement'
source_two="$(git -C "$repositories/canonical seed" rev-parse HEAD)"
git -C "$repositories/canonical seed" push -q
git -C "$source" pull -q --ff-only
mirror_two="$(sync_and_push)"
assert_equal true "$(output_value build_required)" 'source-advance build output'
git -C "$target" merge-base --is-ancestor "$mirror_one" "$mirror_two" || fail 'source advance is not fast-forward'
assert_snapshot "$source_two" "$mirror_two"
assert_equal 'Incoming Owner' "$(git -C "$target" log -1 --format=%an "$mirror_two^2")" 'preserved canonical author'
assert_equal $'pub fn optional_python_bridge() {}\n// owner improvement' "$(git -C "$target" show "$mirror_two:crates/pcmflux/src/python.rs")" 'advanced source content'

# An empty canonical commit updates exact source metadata without rebuilding unchanged code.
git -C "$repositories/canonical seed" commit -q --allow-empty -m 'Canonical metadata only'
source_three="$(git -C "$repositories/canonical seed" rev-parse HEAD)"
git -C "$repositories/canonical seed" push -q
git -C "$source" pull -q --ff-only
mirror_three="$(sync_and_push)"
assert_equal true "$(output_value changed)" 'metadata-only snapshot change'
assert_equal false "$(output_value build_required)" 'metadata-only build output'
assert_snapshot "$source_three" "$mirror_three"

# Direct tracked edits are rejected by content comparison, not by echoing a guard.
git clone -q --branch monorepo-import "$repositories/component.remote" "$repositories/human clone"
human="$repositories/human clone"
git -C "$human" config user.name 'Human Editor'
git -C "$human" config user.email 'human@example.invalid'
printf '// direct mirror edit\n' >> "$human/crates/pcmflux/src/python.rs"
commit_all "$human" 'Direct mirror edit requiring canonical incorporation'
human_edit="$(git -C "$human" rev-parse HEAD)"
git -C "$human" push -q
fetch_mirror
expect_failure run_helper
assert_equal "$human_edit" "$(git -C "$target" rev-parse refs/remotes/origin/monorepo-import)" 'human edit branch identity after refusal'
assert_equal $'pub fn optional_python_bridge() {}\n// owner improvement\n// direct mirror edit' "$(git -C "$target" show "refs/remotes/origin/monorepo-import:crates/pcmflux/src/python.rs")" 'human content after refusal'
git -C "$human" revert --no-edit HEAD >/dev/null
git -C "$human" push -q
fetch_mirror
mirror_after_revert="$(sync_and_push)"
assert_snapshot "$source_three" "$mirror_after_revert"
git -C "$target" merge-base --is-ancestor "$human_edit" "$mirror_after_revert" || fail 'human ancestry was discarded'

# Extra committed paths are also refused and preserved.
printf 'unexpected\n' > "$human/extra-component-path.txt"
commit_all "$human" 'Unexpected mirror path'
human_extra="$(git -C "$human" rev-parse HEAD)"
git -C "$human" push -q
fetch_mirror
expect_failure run_helper
assert_equal "$human_extra" "$(git -C "$target" rev-parse refs/remotes/origin/monorepo-import)" 'extra-path branch identity after refusal'
assert_equal unexpected "$(git -C "$target" show "refs/remotes/origin/monorepo-import:extra-component-path.txt")" 'extra-path content after refusal'
git -C "$human" revert --no-edit HEAD >/dev/null
git -C "$human" push -q
fetch_mirror
sync_and_push >/dev/null

# An untracked target conflict stops the helper before any working-tree overwrite.
mkdir -p -- "$target/crates/pcmflux"
printf 'keep untracked\n' > "$target/crates/pcmflux/untracked.txt"
before_untracked="$(git -C "$target" rev-parse monorepo-import)"
expect_failure run_helper
assert_equal "$before_untracked" "$(git -C "$target" rev-parse monorepo-import)" 'untracked-conflict branch identity'
assert_equal 'keep untracked' "$(cat "$target/crates/pcmflux/untracked.txt")" 'untracked-conflict content'
rm -rf -- "$target/crates"

# The default-branch controller is the only non-metadata mirror-owned overlay.
printf '# controller update\n' >> "$target/.github/workflows/monorepo-transition.yml"
commit_all "$target" 'Update transition controller'
git -C "$target" push -q origin main
mirror_controller="$(sync_and_push)"
assert_equal true "$(output_value changed)" 'controller update snapshot change'
assert_equal false "$(output_value build_required)" 'controller update build output'
assert_equal "$(git -C "$target" show main:.github/workflows/monorepo-transition.yml)" "$(git -C "$target" show "$mirror_controller:.github/workflows/monorepo-transition.yml")" 'controller overlay content'
assert_snapshot "$(git -C "$source" rev-parse HEAD)" "$mirror_controller"
assert_equal "$mirror_controller" "$(sync_and_push)" 'final no-op commit'

# A reviewed merge puts the exact consolidated tree and both histories on main.
run_promoter monorepo-import "$mirror_controller" "$source_three"
assert_equal open-or-update "$(output_value action)" 'pre-merge PR planning'
git -C "$target" merge -q --no-ff monorepo-import -m 'Merge reviewed canonical component snapshot'
git -C "$target" push -q origin main
git -C "$target" merge-base --is-ancestor "$source_three" main || fail 'merged main lost canonical source ancestry'
assert_equal "$(git -C "$target" rev-parse "$mirror_controller^{tree}")" "$(git -C "$target" rev-parse 'main^{tree}')" 'merged main tree'
run_promoter monorepo-import "$mirror_controller" "$source_three"
assert_equal none "$(output_value action)" 'post-merge PR idempotence'
assert_equal up-to-date "$(output_value state)" 'post-merge promotion state'
assert_equal "$mirror_controller" "$(sync_and_push)" 'post-merge snapshot no-op'

# After cutover, canonical main drives the same staging branch without stale metadata.
git -C "$repositories/canonical seed" switch -q -c main
git -C "$repositories/canonical seed" push -q -u origin main
git -C "$source" fetch -q origin main
git -C "$source" switch -q -c main --track origin/main
mirror_cutover="$(sync_and_push main)"
assert_equal false "$(output_value build_required)" 'source-ref-only cutover build output'
assert_snapshot "$source_three" "$mirror_cutover" main
printf '// post-cutover owner change\n' >> "$repositories/canonical seed/crates/pcmflux/src/python.rs"
commit_all "$repositories/canonical seed" 'Post-cutover canonical main change'
source_four="$(git -C "$repositories/canonical seed" rev-parse HEAD)"
git -C "$repositories/canonical seed" push -q
git -C "$source" pull -q --ff-only
mirror_four="$(sync_and_push main)"
assert_equal true "$(output_value build_required)" 'post-cutover source build output'
assert_snapshot "$source_four" "$mirror_four" main
run_promoter main "$mirror_four" "$source_four"
assert_equal open-or-update "$(output_value action)" 'post-cutover PR planning'
git -C "$target" merge -q --no-ff monorepo-import -m 'Merge reviewed canonical main update'
git -C "$target" push -q origin main
run_promoter main "$mirror_four" "$source_four"
assert_equal none "$(output_value action)" 'post-cutover merged PR idempotence'
assert_equal "$mirror_four" "$(sync_and_push main)" 'canonical main no-op after merged PR'

printf 'PASS initial=%s advanced=%s metadata=%s final=%s cutover=%s merged=%s\n' \
    "$mirror_one" "$mirror_two" "$mirror_three" "$mirror_controller" "$mirror_cutover" "$mirror_four"
