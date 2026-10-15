#!/usr/bin/env bash
# Prepare the release PR for a tag: prepend the changelog section with its compare link, bump the
# version, and pin the install snippets to the tag.
#
#   ./scripts/prepare-release.sh vX.Y.Z [highlights.md]
#
# A highlights file renders at the top of the changelog section. Annotate the tag with the same
# file (`git tag -a --cleanup=verbatim -F highlights.md vX.Y.Z`) so regenerating the section
# reproduces it.

set -euo pipefail

if [[ "$#" -lt 1 || "$#" -gt 2 ]]; then
  echo "usage: $0 <vX.Y.Z> [highlights-file]" >&2
  exit 2
fi

tag="$1"
highlights="${2:-}"
if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]]; then
  echo "Error: expected a tag such as v1.2.3 or v1.2.3-rc.1 (got: $tag)" >&2
  exit 1
fi
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "Error: tag $tag already exists" >&2
  exit 1
fi

cd "$(git rev-parse --show-toplevel)"
files=(CHANGELOG.md Cargo.toml Cargo.lock README.md docs/guide/install.md)
if ! git diff --quiet HEAD -- "${files[@]}"; then
  echo "Error: discard the changes to ${files[*]} before preparing a release" >&2
  exit 1
fi

version="${tag#v}"
repo="https://github.com/hakula139/oxide-code"

# A stable section rolls up every change since the previous stable (cliff.toml `tag_pattern`), so
# it compares against that tag. A prerelease section lists only its increment.
cliff_args=(--unreleased --tag "$tag")
describe_args=(--tags --abbrev=0 --match 'v[0-9]*')
if [[ "$version" == *-* ]]; then
  cliff_args+=(--tag-pattern '^v[0-9]+\.')
else
  describe_args+=(--exclude '*-*')
fi
if ! git cliff "${cliff_args[@]}" | grep -q '^### '; then
  echo "Error: no changelog entries since the previous tag" >&2
  exit 1
fi

if [[ -n "$highlights" ]]; then
  # The `=` form keeps a message that starts with a `-` bullet from parsing as a flag.
  cliff_args+=("--with-tag-message=$(<"$highlights")")
fi
git cliff "${cliff_args[@]}" --prepend CHANGELOG.md

if previous=$(git describe "${describe_args[@]}" 2>/dev/null); then
  link="[$version]: $repo/compare/$previous..$tag"
else
  link="[$version]: $repo/releases/tag/$tag"
fi
awk -v link="$link" '!done && /^\[[0-9]+\.[0-9]+\.[0-9]+/ { print link; done = 1 } { print }' CHANGELOG.md >CHANGELOG.md.tmp
mv CHANGELOG.md.tmp CHANGELOG.md

perl -pi -e "s/^version = \".*\"/version = \"$version\"/ if /^\[workspace\.package\]$/.../^\[/" Cargo.toml
if ! grep -qx "version = \"$version\"" Cargo.toml; then
  echo "Error: found no workspace.package.version in Cargo.toml to bump" >&2
  exit 1
fi
cargo update --workspace --quiet

perl -pi -e "s/^VERSION=v.*/VERSION=$tag/" README.md docs/guide/install.md

printf 'Prepared %s. Review the diff, then commit it as chore(release): %s.\n' "$tag" "$tag"
