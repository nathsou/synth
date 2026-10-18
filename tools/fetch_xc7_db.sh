#!/usr/bin/env bash
# Pinned CC0 Project X-Ray data. No external implementation tools at runtime.
set -euo pipefail
out=${1:?usage: fetch_xc7_db.sh OUT_DIR}
rev=0a0addedd73e7e4139d52a6d8db4258763e0f1f3
if [[ -e "$out" ]]; then
  if [[ $(git -C "$out" rev-parse HEAD) != "$rev" ]] || [[ -n $(git -C "$out" status --porcelain) ]]; then
    echo "Refusing to replace an existing or modified database: $out" >&2
    exit 1
  fi
else
  git init "$out"
  git -C "$out" remote add origin https://github.com/f4pga/prjxray-db.git
  git -C "$out" fetch --depth 1 origin "$rev"
  git -C "$out" checkout --detach FETCH_HEAD
fi
printf 'Artix-7 database: %s/artix7 (%s)\n' "$out" "$rev"
