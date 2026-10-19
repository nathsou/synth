#!/usr/bin/env bash
# Pinned f4pga fasm2bels for tools/ci_artix7_bitsim.py (Apache-2.0/ISC test
# oracles; never invoked by the synth executable). Needs the Project X-Ray
# checkout from tools/prepare_xc7_oracle.sh for its Python database reader.
set -euo pipefail
out=${1:?usage: prepare_fasm2bels.sh OUT_DIR XRAY_CHECKOUT}
xray=${2:?usage: prepare_fasm2bels.sh OUT_DIR XRAY_CHECKOUT}
mkdir -p "$out"
out=$(cd "$out" && pwd)
fasm2bels=bafbcd8727e0807d6a620d1cf5dd151111c4a322
rr_graph=b8f0ebb6fd9842ae8bb7ff51a0adc5f76a853802
interchange=04a02101d1f7f03a2d33716192fb478e1e8605af
if [[ ! -e "$out/fasm2bels" ]]; then
  git init "$out/fasm2bels"
  git -C "$out/fasm2bels" remote add origin https://github.com/chipsalliance/f4pga-xc-fasm2bels.git
  git -C "$out/fasm2bels" fetch --depth 1 origin "$fasm2bels"
  git -C "$out/fasm2bels" checkout --detach FETCH_HEAD
fi
if [[ $(git -C "$out/fasm2bels" rev-parse HEAD) != "$fasm2bels" ]]; then
  echo "Unexpected fasm2bels revision; refusing to change an existing checkout" >&2
  exit 1
fi
python3 -m venv "$out/venv"
"$out/venv/bin/pip" install -r "$(dirname "$0")/xc7-test-requirements.txt" \
  progressbar2==4.6.0 lxml==6.1.3 pycapnp==2.2.4 pyjson5==2.0.1 Jinja2==3.1.6 numpy==2.5.4
# The fasm2bels package metadata names unpinned Git dependencies; install the
# pinned revisions without letting pip resolve them again.
"$out/venv/bin/pip" install --no-deps -e "$xray" -e "$out/fasm2bels" \
  "git+https://github.com/chipsalliance/f4pga-rr-graph.git@$rr_graph" \
  "git+https://github.com/chipsalliance/python-fpga-interchange.git@$interchange"
printf 'fasm2bels: %s/venv/bin/python\n' "$out"
