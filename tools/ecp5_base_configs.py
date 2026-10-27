#!/usr/bin/env python3
"""Converts nextpnr's ecp5/baseconfigs.cc (the configuration of an empty design
on each ECP5 device) into Project Trellis textual configurations:

    tools/ecp5_base_configs.py <nextpnr>/ecp5/baseconfigs.cc crates/arch-ecp5/data/base
"""
import re, sys, collections
src, outdir = sys.argv[1], sys.argv[2]
cur = None
devices = collections.OrderedDict()
for line in open(src):
    m = re.match(r'void config_empty_(\w+)\(ChipConfig &cc\)', line)
    if m:
        cur = {"name": None, "tiles": collections.OrderedDict()}
        devices[m.group(1)] = cur
        continue
    if cur is None:
        continue
    m = re.search(r'cc\.chip_name = "([^"]+)"', line)
    if m:
        cur["name"] = m.group(1)
        continue
    m = re.search(r'cc\.tiles\["([^"]+)"\]\.add_(\w+)\((.*)\);', line)
    if m:
        tile, kind, args = m.groups()
        t = cur["tiles"].setdefault(tile, [])
        if kind == "enum":
            a = re.findall(r'"([^"]*)"', args); t.append("enum: %s %s" % (a[0], a[1]))
        elif kind == "arc":
            a = re.findall(r'"([^"]*)"', args); t.append("arc: %s %s" % (a[0], a[1]))
        elif kind == "unknown":
            f, b = [int(x) for x in args.split(",")]; t.append("unknown: F%dB%d" % (f, b))
        else:
            raise SystemExit("unexpected " + kind)
for d in devices.values():
    out = [".device %s" % d["name"], ""]
    for tile, entries in d["tiles"].items():
        out.append(".tile " + tile)
        out.extend(entries)
        out.append("")
    open("%s/%s.config" % (outdir, d["name"]), "w").write("\n".join(out))
    print(d["name"], len(d["tiles"]))
