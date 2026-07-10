#!/usr/bin/env bash
# Assert an ELF binary's required GLIBC symbols stay at or below a floor.
#
# Usage: assert-max-glibc.sh <binary> <max_version>
# Example: assert-max-glibc.sh target/release/mosh-client 2.28
#
# Netcatty packaging floors (must stay in sync with Netcatty CI):
#   linux-x64   → glibc 2.28  (almalinux:8)
#   linux-arm64 → glibc 2.31  (debian:bullseye)
set -euo pipefail

BIN="${1:?binary path required}"
MAX="${2:?max glibc version required, e.g. 2.28}"

if [[ ! -f "$BIN" ]]; then
  echo "ERROR: binary not found: $BIN" >&2
  exit 1
fi

if ! command -v strings >/dev/null 2>&1; then
  echo "ERROR: strings(1) required" >&2
  exit 1
fi

version_le() {
  # return 0 if $1 <= $2 (dotted numeric)
  local a="$1" b="$2"
  local IFS=.
  # shellcheck disable=SC2206
  local av=($a) bv=($b)
  local i n=${#av[@]}
  if [[ ${#bv[@]} -gt $n ]]; then n=${#bv[@]}; fi
  for ((i = 0; i < n; i++)); do
    local x=${av[i]:-0} y=${bv[i]:-0}
    if ((10#$x < 10#$y)); then return 0; fi
    if ((10#$x > 10#$y)); then return 1; fi
  done
  return 0
}

VERS_RAW=$(strings "$BIN" | grep -oE 'GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?' | sed 's/^GLIBC_//' | sort -u || true)

if [[ -z "$VERS_RAW" ]]; then
  echo "WARN: no GLIBC_* symbols found in $BIN (static? musl?)"
  exit 0
fi

MAX_SEEN=""
while IFS= read -r v; do
  [[ -z "$v" ]] && continue
  if [[ -z "$MAX_SEEN" ]] || ! version_le "$v" "$MAX_SEEN"; then
    MAX_SEEN="$v"
  fi
done <<< "$VERS_RAW"

echo "GLIBC symbols in $BIN:"
echo "$VERS_RAW" | sed 's/^/  /'
echo "max required: $MAX_SEEN (floor: $MAX)"

if ! version_le "$MAX_SEEN" "$MAX"; then
  echo "ERROR: $BIN requires GLIBC_$MAX_SEEN > floor GLIBC_$MAX" >&2
  echo "Build on a container matching Netcatty's Linux compatibility floor." >&2
  exit 1
fi

echo "OK: glibc requirement $MAX_SEEN <= $MAX"
