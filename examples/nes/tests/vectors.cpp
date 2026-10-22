// SPDX-License-Identifier: CERN-OHL-W-2.0
// Initialize state through real instructions, then check every external cycle
// of a single instruction against independent nes6502 test vectors.
#include "Vnes_cpu_test.h"
#include "verilated.h"
#include <array>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <vector>

static unsigned u8(std::istream &f) { int v = f.get(); if (v < 0) throw std::runtime_error("truncated vectors"); return unsigned(v); }
static unsigned u16(std::istream &f) { unsigned lo = u8(f); return lo | (u8(f) << 8); }
static unsigned u32(std::istream &f) { unsigned lo = u16(f); return lo | (u16(f) << 16); }
struct State {
    unsigned pc, a, x, y, p, s;
    explicit State(std::istream &f) : pc(u16(f)), a(u8(f)), x(u8(f)), y(u8(f)), p(u8(f)), s(u8(f)) {}
    bool matches(const Vnes_cpu_test &cpu) const {
        // B is a synthesized stack bit, not a physical status latch. The
        // vectors retain it in their software status representation.
        return pc == cpu.debug_pc && a == cpu.debug_a && x == cpu.debug_x && y == cpu.debug_y &&
            (p & 0xef) == (cpu.debug_p & 0xef) && s == cpu.debug_s;
    }
};
struct Byte { unsigned address, value; };
static std::vector<Byte> read_ram(std::istream &f) {
    unsigned count = u16(f); std::vector<Byte> ram;
    while (count--) { unsigned a = u16(f), d = u8(f); ram.push_back({a, d}); }
    return ram;
}
struct Cycle { unsigned address, data, write; };

int main(int argc, char **argv) {
    try {
        if (argc != 2) throw std::runtime_error("usage: cpu_vectors packed-vectors.bin");
        Verilated::commandArgs(argc, argv);
        std::ifstream f(argv[1], std::ios::binary);
        char magic[4]; f.read(magic, 4);
        if (!f || std::string(magic, 4) != "NST1") throw std::runtime_error("bad vector header");
        const unsigned count = u32(f);
        Vnes_cpu_test cpu;
        std::array<uint8_t, 65536> memory{};
        auto settle = [&]() { cpu.clk = 0; cpu.eval(); cpu.data = memory[cpu.address]; cpu.eval(); };
        auto edge = [&]() { if (cpu.write) memory[cpu.address] = cpu.write_data; cpu.clk = 1; cpu.eval(); };
        for (unsigned test = 0; test < count; ++test) {
            const unsigned opcode = u8(f), index = u32(f);
            const State initial(f);
            const auto before = read_ram(f);
            std::vector<Cycle> cycles;
            unsigned cycle_count = u8(f);
            while (cycle_count--) { unsigned a = u16(f), d = u8(f), w = u8(f); cycles.push_back({a, d, w}); }
            const State final(f);
            const auto after = read_ram(f);
            memory.fill(0);
            // PLP is last so setup instructions cannot disturb N/Z/C/V/D/I.
            const uint8_t setup[] = {0xa2, uint8_t(initial.s), 0x9a, 0xa2, uint8_t(initial.x), 0xa0, uint8_t(initial.y),
                0xa9, uint8_t(initial.p), 0x48, 0xa9, uint8_t(initial.a), 0x28, 0x4c, uint8_t(initial.pc), uint8_t(initial.pc >> 8)};
            std::copy(std::begin(setup), std::end(setup), memory.begin() + 0xc000);
            memory[0xfffc] = 0; memory[0xfffd] = 0xc0;
            cpu.enable = 1; cpu.nmi = 0; cpu.irq = 0; cpu.reset = 1; settle(); edge(); cpu.reset = 0;
            unsigned fetched = 0, setup_cycles = 0;
            do {
                settle();
                if (cpu.sync_fetch && fetched++ == 9) break;
                edge();
                if (++setup_cycles > 100) throw std::runtime_error("setup failed");
            } while (true);
            if (!initial.matches(cpu)) {
                std::fprintf(stderr, "setup opcode %02X vector %u expected PC:%04X A:%02X X:%02X Y:%02X P:%02X SP:%02X; actual PC:%04X A:%02X X:%02X Y:%02X P:%02X SP:%02X\n",
                    opcode, index, initial.pc, initial.a, initial.x, initial.y, initial.p, initial.s,
                    cpu.debug_pc, cpu.debug_a, cpu.debug_x, cpu.debug_y, cpu.debug_p, cpu.debug_s);
                throw std::runtime_error("initial register setup failed");
            }
            for (auto b : before) memory[b.address] = b.value;
            bool mismatch = false;
            for (unsigned i = 0; i < cycles.size(); ++i) {
                settle(); const auto &c = cycles[i];
                const unsigned value = cpu.write ? cpu.write_data : cpu.data;
                if (cpu.address != c.address || cpu.write != c.write || value != c.data) {
                    std::fprintf(stderr, "opcode %02X vector %u cycle %u: expected %04X %c %02X; actual %04X %c %02X\n",
                        opcode, index, i, c.address, c.write ? 'W' : 'R', c.data, cpu.address, cpu.write ? 'W' : 'R', value);
                    mismatch = true; break;
                }
                edge();
            }
            settle();
            if (!final.matches(cpu)) {
                std::fprintf(stderr, "opcode %02X vector %u final expected PC:%04X A:%02X X:%02X Y:%02X P:%02X SP:%02X; actual PC:%04X A:%02X X:%02X Y:%02X P:%02X SP:%02X\n",
                    opcode, index, final.pc, final.a, final.x, final.y, final.p, final.s,
                    cpu.debug_pc, cpu.debug_a, cpu.debug_x, cpu.debug_y, cpu.debug_p, cpu.debug_s);
                mismatch = true;
            }
            for (auto b : after) if (memory[b.address] != b.value) mismatch = true;
            if (mismatch) return 1;
        }
        if (f.peek() != std::char_traits<char>::eof()) throw std::runtime_error("trailing vector data");
        std::cout << "PASS " << count << " independent CPU vectors: every bus read/write and final state matches\n";
        cpu.final(); return 0;
    } catch (const std::exception &e) { std::cerr << e.what() << '\n'; return 1; }
}
