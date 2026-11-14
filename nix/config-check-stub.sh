#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "stub: $1" >&2
  exit 1
}

[ "$*" = "config check" ] || fail "expected config check"
[ -d "$XDG_CONFIG_HOME/ox" ] || fail "expected the config directory"
[ -z "$(ls -A "$PWD")" ] || fail "expected an empty working directory"
[ -d "$HOME" ] && [ -w "$HOME" ] && [ -z "$(ls -A "$HOME")" ] \
  || fail "expected an empty writable home directory"
[ -n "$XDG_DATA_HOME" ] && [ -n "$XDG_STATE_HOME" ] || fail "expected data and state dirs"
if grep -q invalid "$XDG_CONFIG_HOME/ox/config.toml"; then
  fail "invalid config"
fi
