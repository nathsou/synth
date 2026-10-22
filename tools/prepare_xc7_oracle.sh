#!/usr/bin/env bash
# Build the pinned, independent frame/packet test oracles. Requires git, a C++
# compiler, CMake, and Python with venv. Never invoked by the synth executable.
set -euo pipefail
out=${1:?usage: prepare_xc7_oracle.sh OUT_DIR}
mkdir -p "$out"
out=$(cd "$out" && pwd)
rev=c9f02d8576042325425824647ab5555b1bc77833
if [[ ! -e "$out/prjxray" ]]; then
  git init "$out/prjxray"
  git -C "$out/prjxray" remote add origin https://github.com/f4pga/prjxray.git
  git -C "$out/prjxray" fetch --depth 1 origin "$rev"
  git -C "$out/prjxray" checkout --detach FETCH_HEAD
fi
if [[ $(git -C "$out/prjxray" rev-parse HEAD) != "$rev" ]]; then
  echo "Unexpected X-Ray revision; refusing to change an existing checkout" >&2
  exit 1
fi
git -C "$out/prjxray" submodule update --init --recursive --depth 1
python3 -m venv "$out/venv"
"$out/venv/bin/pip" install -r "$(dirname "$0")/xc7-test-requirements.txt"
cmake -S "$out/prjxray" -B "$out/build" -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DCMAKE_BUILD_TYPE=Release
cmake --build "$out/build" --target xc7frames2bit bitread -j2
