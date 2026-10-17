# iCE40 chip databases

Binary databases embedded by `arch-ice40` (`ChipDb::get`). They are generated,
not hand-written; regenerate them when the upstream data changes.

## Sources

- Project IceStorm (ISC licence), <https://github.com/YosysHQ/icestorm>:
  - chip databases from `icebox_chipdb` (IceStorm 2023-02-18 as packaged by
    Ubuntu 24.04, `fpga-icestorm 0~20230218gitd20a5e9-1`);
  - timing data from `icefuzz/timings_*.txt` (upstream commit b77a2d5).

| File | Command |
|---|---|
| `chipdb-384.bin` | `icebox_chipdb -3`, `timings_lp384.txt` |
| `chipdb-1k.bin` | `icebox_chipdb`, `timings_hx1k.txt` |
| `chipdb-8k.bin` | `icebox_chipdb -8`, `timings_hx8k.txt` |

## Regenerating

```sh
icebox_chipdb -8 > chipdb-8k.txt
cargo run --release -p synth-dbgen -- ice40 chipdb-8k.txt \
    icestorm/icefuzz/timings_hx8k.txt crates/arch-ice40/db/chipdb-8k.bin
```

The importer checks that the templated routing reproduces every switch of the
source exactly, and fails otherwise.
