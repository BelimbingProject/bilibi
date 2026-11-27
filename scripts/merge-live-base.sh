#!/usr/bin/env bash
# Merge the base into GitHub's PR merge ref. See docs/domain-ci/README.md for
# when mounted jobs use a preselected commit or fetch the live tip.
# Usage: merge-live-base.sh <base-branch> [selected-base-commit]
set -euo pipefail
base=${1:?usage: merge-live-base.sh <base-branch> [selected-base-commit]}
selected_tip=${2:-}

if [[ -n "$selected_tip" ]]; then
  tip=$selected_tip
  git fetch --quiet --no-tags origin "$tip"
else
  git fetch --quiet --no-tags origin "refs/heads/$base"
  tip=$(git rev-parse FETCH_HEAD)
fi

if git merge-base --is-ancestor "$tip" HEAD; then
  echo "This checkout already contains the live ${base} (${tip:0:12})."
  exit 0
fi

echo "Merging the live ${base} (${tip:0:12}) into this checkout (${base} moved after the merge ref was made)."
merge_env=()
if [[ -n "$selected_tip" ]]; then
  merge_date=$(git show -s --format=%cI "$tip")
  merge_env=("GIT_AUTHOR_DATE=$merge_date" "GIT_COMMITTER_DATE=$merge_date")
fi
if ! env "${merge_env[@]}" git -c user.name=github-actions -c user.email=github-actions@github.com \
  merge --quiet --no-edit "$tip"; then
  git merge --abort 2>/dev/null || true
  message="This pull request conflicts with the live ${base}, which moved after the pull request was last updated. Update the branch (merge or rebase ${base}), resolve the conflicts and push; the check runs again on the result."
  echo "::error::${message}"
  echo "$message" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
  exit 1
fi
echo "Now testing $(git rev-parse HEAD | cut -c1-12), which is the pull request on top of the live ${base}."
