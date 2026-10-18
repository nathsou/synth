# Artix-7 data and implementation references

The runtime backend is native Rust. Project X-Ray's device descriptions are
external CC0 data; `tools/fetch_xc7_db.sh` pins f4pga/prjxray-db to
`0a0addedd73e7e4139d52a6d8db4258763e0f1f3`. Retain that repository's COPYING file
when redistributing the data. No Xilinx proprietary device files are included.

The Series-7 ECC algorithm, packet startup sequence, frame address progression,
and FASM bit addressing were cross-checked against Project X-Ray at
`c9f02d8576042325425824647ab5555b1bc77833` (https://github.com/f4pga/prjxray).
Site mux and clock configuration were cross-checked against nextpnr-xilinx at
`bc9b234634cf80756b04ad4ea5f8e7a83293c425`
(https://github.com/openXC7/nextpnr-xilinx). The Rust implementation references
these ISC-licensed projects; neither executable is a runtime dependency.
Their applicable notices are reproduced below.

Copyright (C) 2017-2020 The Project X-Ray Authors.

Copyright (C) 2018 Clifford Wolf <clifford@symbioticeda.com>
Copyright (C) 2018-2019 David Shah <david@symbioticeda.com>
Copyright (C) 2018 Dan Gisselquist <dan@symbioticeda.com>
Copyright (C) 2018 Serge Bazanski <q3k@symbioticeda.com>
Copyright (C) 2018 Miodrag Milanovic <miodrag@symbioticeda.com>
Copyright (C) 2018 Eddie Hung <eddieh@ece.ubc.ca>
Copyright (C) 2023 Hans Baier <hansfbaier@gmail.com>

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

The independently downloaded Xilinx RAMB18E1 simulation model is Apache-2.0.
It is a test dependency only, fetched at the revision in
`tools/fetch_xc7_models.sh`, and is not distributed with synth.
