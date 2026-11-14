#!/usr/bin/env bash
set -euo pipefail

# Follow the latest release, or the newest prerelease before the first stable one.
latest=$(
  gh api graphql \
    -F owner='{owner}' \
    -F name='{repo}' \
    -f query="query(\$owner: String!, \$name: String!) {
      repository(owner: \$owner, name: \$name) {
        latestRelease { tagName }
      }
    }" \
    --jq '.data.repository.latestRelease.tagName // empty'
)

if [[ -z "$latest" ]]; then
  latest=$(
    gh release list \
      --exclude-drafts \
      --limit 1 \
      --json tagName \
      --jq '.[0].tagName // empty'
  )
fi

if [[ -z "$latest" ]]; then
  echo '::error::No published release found for the Homebrew formula'
  exit 1
fi

if [[ "$latest" != "$GITHUB_REF_NAME" ]]; then
  echo "Skipping $GITHUB_REF_NAME, since the formula follows $latest."
  exit 0
fi

formula=$1
branch="chore/homebrew-$GITHUB_REF_NAME"
title="chore(release): refresh Homebrew formula for $GITHUB_REF_NAME"
existing=false

if git ls-remote \
  --exit-code \
  origin "refs/heads/$branch" \
  >/dev/null; then
  existing=true
  git fetch origin "refs/heads/$branch"
  git switch -c "$branch" FETCH_HEAD
else
  rc=$?
  if [[ "$rc" != 2 ]]; then
    exit "$rc"
  fi
  git switch -c "$branch"
fi

cp "$formula" Formula/oxide-code.rb

if git diff --quiet -- Formula/oxide-code.rb; then
  echo "The formula already points at $GITHUB_REF_NAME."
  if [[ "$existing" == false ]]; then
    exit 0
  fi
else
  git \
    -c user.name='hakula-release-bot[bot]' \
    -c user.email='351352052+hakula-release-bot[bot]@users.noreply.github.com' \
    commit \
    -m "$title" \
    -- Formula/oxide-code.rb
  git push origin "$branch"
fi

if [[ -z "$(
  gh pr list \
    --head "$branch" \
    --state open \
    --json number \
    --jq '.[].number'
)" ]] \
  && ! gh pr create \
    --base main \
    --head "$branch" \
    --title "$title" \
    --assignee hakula139 \
    --label chore \
    --body-file .github/templates/homebrew-formula-pr.md; then
  echo "::error::Could not open the formula PR."
  exit 1
fi
