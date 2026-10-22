// SPDX-License-Identifier: CERN-OHL-W-2.0
#include "Vnes_cpu_test.h"
#include "verilated.h"
#include <array>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

int main(int argc, char **argv) {
    try {
        if (argc != 3) throw std::runtime_error("usage: cpu_test nestest.nes nestest.log");
        Verilated::commandArgs(argc, argv);
        std::ifstream rom(argv[1], std::ios::binary);
        std::vector<uint8_t> bytes((std::istreambuf_iterator<char>(rom)), {});
        if (bytes.size() < 16 + 16384) throw std::runtime_error("truncated nestest ROM");
        std::array<uint8_t, 65536> memory{};
        for (unsigned i = 0x8000; i <= 0xffff; ++i) memory[i] = bytes[16 + ((i - 0x8000) % (bytes[4] * 16384))];
        memory[0xfffc] = 0; memory[0xfffd] = 0xc0;
        Vnes_cpu_test cpu;
        cpu.enable = 1; cpu.nmi = 0; cpu.irq = 0; cpu.reset = 1;
        cpu.clk = 0; cpu.eval(); cpu.clk = 1; cpu.eval();
        cpu.reset = 0;
        std::ifstream log(argv[2]);
        std::string line;
        uint64_t cycles = 0, instructions = 0;
        while (std::getline(log, line)) {
            unsigned pc, a, x, y, p, s, expected_cycle;
            if (std::sscanf(line.c_str(), "%x", &pc) != 1) throw std::runtime_error("bad log PC");
            auto regs = line.find("A:");
            auto clock = line.find("CYC:");
            if (regs == std::string::npos || clock == std::string::npos ||
                std::sscanf(line.c_str() + regs, "A:%x X:%x Y:%x P:%x SP:%x", &a, &x, &y, &p, &s) != 5 ||
                std::sscanf(line.c_str() + clock, "CYC:%u", &expected_cycle) != 1) throw std::runtime_error("bad log registers");
            do {
                cpu.clk = 0; cpu.eval();
                auto addr = cpu.address;
                if (cpu.write) memory[addr < 0x2000 ? (addr & 0x7ff) : addr] = cpu.write_data;
                cpu.data = memory[addr < 0x2000 ? (addr & 0x7ff) : addr]; cpu.eval();
                if (cpu.sync_fetch) break;
                cpu.clk = 1; cpu.eval(); ++cycles;
                if (cycles > 1000000) throw std::runtime_error("CPU stopped fetching");
            } while (true);
            if (cpu.debug_pc != pc || cpu.debug_a != a || cpu.debug_x != x || cpu.debug_y != y ||
                cpu.debug_p != p || cpu.debug_s != s || cycles != expected_cycle) {
                std::fprintf(stderr, "Mismatch at instruction %llu\n%s\nactual PC:%04X A:%02X X:%02X Y:%02X P:%02X SP:%02X CYC:%llu\n",
                    (unsigned long long)instructions, line.c_str(), cpu.debug_pc, cpu.debug_a, cpu.debug_x,
                    cpu.debug_y, cpu.debug_p, cpu.debug_s, (unsigned long long)cycles);
                return 1;
            }
            cpu.clk = 1; cpu.eval(); ++cycles; ++instructions;
        }
        if (instructions != 8991) throw std::runtime_error("incomplete nestest log");
        std::cout << "PASS nestest: " << instructions << " instructions, registers and cycle counts match\n";
        cpu.final();
        return 0;
    } catch (const std::exception &e) { std::cerr << e.what() << '\n'; return 1; }
}
