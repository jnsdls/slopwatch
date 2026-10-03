#!/usr/bin/env bash
# Fails if any workspace crate other than the client pulls in GPUI or
# gpui-kit, directly or through another dependency (ADR 0005).
set -euo pipefail

cd "$(dirname "$0")/.."

client=slopwatch-client
status=0

crates=$(cargo metadata --no-deps --format-version 1 |
  python3 -c 'import json, sys; print("\n".join(p["name"] for p in json.load(sys.stdin)["packages"]))')

for crate in $crates; do
  [[ "$crate" == "$client" ]] && continue
  deps=$(cargo tree -p "$crate" -e normal,build,dev --prefix none --format '{p}')
  gpui=$(awk '{print $1}' <<<"$deps" | grep -E '^gpui' | sort -u || true)
  if [[ -n "$gpui" ]]; then
    echo "error: $crate depends on GPUI crates, and only $client may:" >&2
    echo "$gpui" | sed 's/^/  /' >&2
    echo "  see: cargo tree -p $crate -i <crate>" >&2
    status=1
  else
    echo "ok: $crate has no GPUI crates"
  fi
done

exit "$status"
