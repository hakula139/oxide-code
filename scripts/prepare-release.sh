#!/usr/bin/env bash
# Prepare the release PR for a tag: prepend the draft changelog section with its compare link, bump
# the version, pin the install snippets to the tag, and add the tag's session resume fixture.
#
#   ./scripts/prepare-release.sh vX.Y.Z [highlights.md]
#
# A highlights file lists bullets, which render under a Highlights heading atop the section.

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
snapshots=crates/oxide-code/src/session/snapshots
written="$snapshots/ox__session__handle__tests__session_writes_every_entry_type_as_byte_compatible_jsonl.snap"
fixture="crates/oxide-code/src/session/fixtures/$tag.jsonl"
resumed="$snapshots/ox__session__handle__tests__session_resumes_from_every_release_fixture@$tag.jsonl.snap"
files=(CHANGELOG.md Cargo.toml Cargo.lock README.md docs/guide/install.md "$written")
if ! git diff --quiet HEAD -- "${files[@]}"; then
  echo "Error: discard the changes to ${files[*]} before preparing a release" >&2
  exit 1
fi
for path in "$fixture" "$resumed"; do
  if [[ -e "$path" ]]; then
    echo "Error: remove $path before preparing a release" >&2
    exit 1
  fi
done

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

# The fixture is the byte-compat snapshot without its insta header. `unseen` records the new
# fixture's resume snapshot and still fails when an earlier fixture resumes differently.
awk 'header == 2 { print } /^---$/ { header++ }' "$written" >"$fixture"
INSTA_UPDATE=unseen cargo test --quiet --bin ox session_resumes_from_every_release_fixture

printf 'Prepared %s. Curate the new CHANGELOG.md section, then commit as chore(release): %s.\n' "$tag" "$tag"
