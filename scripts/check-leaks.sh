#!/usr/bin/env bash
# Check that no environment credential appears in ox's logs, session store, or saved tool output.
#
# Run after the TUI checklist in docs/conventions/testing.md, in the same environment. Like the live
# tests, it checks ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY, CLAUDE_CODE_OAUTH_TOKEN, and each
# ANTHROPIC_CUSTOM_HEADERS value of 8 or more characters. It prints only variable names and exits 1
# on a leak and 2 on an error. It needs `sqlite3`, which the dev shell provides.

set -euo pipefail

state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/ox"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/ox"
db="$data_dir/sessions.db"
for path in "$state_dir" "$db"; do
  if [[ ! -e "$path" ]]; then
    echo "$path is missing. Rerun with the XDG directories the sessions used." >&2
    exit 2
  fi
done

# The database splits long values across pages, so search its SQL dump, which also folds in the
# write-ahead log.
dump="$(mktemp)"
trap 'rm -f "$dump"' EXIT
sqlite3 -readonly "$db" .dump >"$dump"
sources=("$state_dir" "$dump")
if [[ -d "$data_dir/artifacts" ]]; then
  sources+=("$data_dir/artifacts")
fi

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
  grep -rqF -f <(printf '%s\n' "${secrets[$i]}") -- "${sources[@]}" || status=$?
  case "$status" in
    0)
      echo "Found the value of ${names[$i]} in $state_dir or $data_dir" >&2
      leaked=1
      ;;
    1) ;;
    *) exit 2 ;;
  esac
done

if [[ "$leaked" -eq 0 ]]; then
  echo "No credential value found in $state_dir or $data_dir"
fi
exit "$leaked"
