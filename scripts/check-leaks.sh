#!/usr/bin/env bash
# Check that no credential from the environment appears in ox's logs or session files.
#
# Run it after the TUI checklist in docs/conventions/testing.md, in the environment the sessions
# used. Like the live tests, it checks ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY,
# CLAUDE_CODE_OAUTH_TOKEN, and each ANTHROPIC_CUSTOM_HEADERS value of 8 or more characters.
# It prints only variable names, and exits 1 on a leak and 2 on an error.

set -euo pipefail

dirs=(
  "${XDG_STATE_HOME:-$HOME/.local/state}/ox"
  "${XDG_DATA_HOME:-$HOME/.local/share}/ox/sessions"
)
for dir in "${dirs[@]}"; do
  if [[ ! -d "$dir" ]]; then
    echo "$dir is missing. Rerun with the XDG directories the sessions used." >&2
    exit 2
  fi
done

secrets=()
names=()
for name in ANTHROPIC_AUTH_TOKEN ANTHROPIC_API_KEY CLAUDE_CODE_OAUTH_TOKEN; do
  if [[ -n "${!name:-}" ]]; then
    secrets+=("${!name}")
    names+=("$name")
  fi
done
while IFS= read -r line; do
  value="${line#*:}"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  if [[ "$line" == *:* && ${#value} -ge 8 ]]; then
    secrets+=("$value")
    names+=("ANTHROPIC_CUSTOM_HEADERS (${line%%:*})")
  fi
done <<<"${ANTHROPIC_CUSTOM_HEADERS:-}"

if [[ "${#secrets[@]}" -eq 0 ]]; then
  echo "No credential variable is set. Export the one the sessions used, then rerun." >&2
  exit 2
fi

leaked=0
for i in "${!secrets[@]}"; do
  status=0
  grep -rqF -- "${secrets[$i]}" "${dirs[@]}" || status=$?
  case "$status" in
    0)
      echo "Found the value of ${names[$i]} in ${dirs[*]}" >&2
      leaked=1
      ;;
    1) ;;
    *) exit 2 ;;
  esac
done

if [[ "$leaked" -eq 0 ]]; then
  echo "No credential value found in ${dirs[*]}"
fi
exit "$leaked"
