# 0004: Licence

Status: Accepted

## Context

The project owner prefers a European licence. The licence must be compatible with
the ISC and CC0 device databases, and must not reach into users' designs.

## Decision

- Toolchain source code: **EUPL-1.2**.
- HDL kept in the repository (example designs, test SoCs, soft IP):
  **CERN-OHL-W-2.0**, marked in each file.
- Output produced by `synth` from users' designs (bitstreams, netlists, reports)
  is not covered by the toolchain licence. The README says so explicitly.

## Consequences

- EUPL-1.2 is copyleft, including for network use. Its compatibility appendix
  allows combination with GPL, LGPL, AGPL, MPL and others.
- Proprietary forks that redistribute modified toolchain code are not allowed;
  using the tool on proprietary designs is.
- Every source file carries `SPDX-License-Identifier` information through the
  workspace manifest (`license = "EUPL-1.2"`).
