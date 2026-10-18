// SPDX-License-Identifier: CERN-OHL-W-2.0
// Only cartridge storage, input and output conversion are implemented here.
// CPU/PPU/APU/controller/DMA/mapper behavior runs in the compiled Weft RTL.
#include "Vnes.h"
#include "verilated.h"
#include <algorithm>
#include <array>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <iterator>
#include <memory>
#include <sstream>
#include <stdexcept>
#include <string>
#include <vector>

static std::vector<uint8_t> read_file(const std::string &name) {
    std::ifstream f(name, std::ios::binary);
    if (!f) throw std::runtime_error("cannot open " + name);
    return {std::istreambuf_iterator<char>(f), {}};
}
struct Cartridge {
    std::vector<uint8_t> prg, chr;
    std::array<uint8_t, 8192> ram{};
    uint8_t mapper;
    bool vertical, four, chr_ram, battery;
    explicit Cartridge(const std::string &name) {
        const auto b = read_file(name);
        if (b.size() < 16 || b[0] != 'N' || b[1] != 'E' || b[2] != 'S' || b[3] != 0x1a)
            throw std::runtime_error("not an iNES cartridge");
        if ((b[7] & 0x0c) == 8) throw std::runtime_error("NES 2.0 is not supported");
        if ((b[7] & 3) || (b[9] & 1)) throw std::runtime_error("only NTSC home NES cartridges are supported");
        mapper = (b[6] >> 4) | (b[7] & 0xf0);
        if (mapper != 0 && mapper != 1 && mapper != 2 && mapper != 3 && mapper != 4 && mapper != 7)
            throw std::runtime_error("unsupported mapper " + std::to_string(mapper));
        const size_t ps = b[4] * 16384u, cs = b[5] * 8192u;
        if (!ps || (ps & (ps - 1)) || (cs && (cs & (cs - 1))) || ps > 2097152 || cs > 262144)
            throw std::runtime_error("unsupported cartridge memory geometry");
        if ((mapper == 0 && (ps > 32768 || cs > 8192)) || (mapper == 1 && ps > 262144))
            throw std::runtime_error("unsupported board variant");
        if ((mapper == 1 && cs > 131072) || (mapper == 2 && (ps > 262144 || cs > 8192)) ||
            (mapper == 3 && ps > 32768) || (mapper == 4 && ps > 524288) ||
            (mapper == 7 && (ps > 262144 || cs != 0))) throw std::runtime_error("unsupported board variant");
        if (b[8] > 1) throw std::runtime_error("banked cartridge RAM is not supported");
        size_t offset = 16 + ((b[6] & 4) ? 512 : 0);
        if (b.size() < offset + ps + cs) throw std::runtime_error("truncated cartridge image");
        if (b[6] & 4) std::copy(b.begin() + 16, b.begin() + 528, ram.begin() + 0x1000);
        prg.assign(b.begin() + offset, b.begin() + offset + ps);
        offset += ps;
        chr_ram = cs == 0;
        if (chr_ram) chr.resize(8192);
        else chr.assign(b.begin() + offset, b.begin() + offset + cs);
        vertical = b[6] & 1; four = b[6] & 8; battery = b[6] & 2;
    }
};

// A fixed RGB palette; analog composite decoding is deliberately outside RTL.
static constexpr uint32_t palette[64] = {
    0x666666,0x002A88,0x1412A7,0x3B00A4,0x5C007E,0x6E0040,0x6C0600,0x561D00,
    0x333500,0x0B4800,0x005200,0x004F08,0x00404D,0x000000,0x000000,0x000000,
    0xADADAD,0x155FD9,0x4240FF,0x7527FE,0xA01ACC,0xB71E7B,0xB53120,0x994E00,
    0x6B6D00,0x388700,0x0C9300,0x008F32,0x007C8D,0x000000,0x000000,0x000000,
    0xFFFEFF,0x64B0FF,0x9290FF,0xC676FF,0xF36AFF,0xFE6ECC,0xFE8170,0xEA9E22,
    0xBCBE00,0x88D800,0x5CE430,0x45E082,0x48CDDE,0x4F4F4F,0x000000,0x000000,
    0xFFFEFF,0xC0DFFF,0xD3D2FF,0xE8C8FF,0xFBC2FF,0xFEC4EA,0xFECCC5,0xF7D8A5,
    0xE4E594,0xCFEF96,0xBDF4AB,0xB3F3CC,0xB5EBF2,0xB8B8B8,0x000000,0x000000,
};
static void put16(std::ostream &f, uint16_t n) { f.put(n & 255); f.put(n >> 8); }
static void put32(std::ostream &f, uint32_t n) { put16(f, n & 65535); put16(f, n >> 16); }
static void wav(const std::string &name, const std::vector<int16_t> &samples) {
    std::ofstream f(name, std::ios::binary);
    if (!f) throw std::runtime_error("cannot write " + name);
    f.write("RIFF", 4); put32(f, 36 + samples.size() * 2); f.write("WAVEfmt ", 8);
    put32(f, 16); put16(f, 1); put16(f, 1); put32(f, 48000); put32(f, 96000);
    put16(f, 2); put16(f, 16); f.write("data", 4); put32(f, samples.size() * 2);
    for (auto s : samples) put16(f, uint16_t(s));
}
struct Input { unsigned frame; unsigned p1; unsigned p2; };

int main(int argc, char **argv) {
    try {
        if (argc < 2) throw std::runtime_error("usage: nes_sim ROM [--frames N] [--test] [--legacy-result HEX_ADDRESS] [--ppm FILE] [--wav FILE] [--input FILE] [--save FILE] [--trace FILE]");
        Verilated::commandArgs(argc, argv);
        std::string ppm_name, wav_name, input_name, save_name, trace_name;
        unsigned max_frames = 600;
        bool test = false;
        bool console_test = false;
        std::vector<std::string> console_crcs;
        int legacy_address = -1, legacy_code = -1;
        for (int i = 2; i < argc; ++i) {
            std::string arg = argv[i];
            if (arg == "--test") { test = true; continue; }
            if (arg == "--console-test") { test = true; console_test = true; continue; }
            if (i + 1 == argc) throw std::runtime_error("missing value for " + arg);
            const std::string value = argv[++i];
            if (arg == "--frames") { max_frames = std::stoul(value); if (!max_frames) throw std::runtime_error("frames must be positive"); }
            else if (arg == "--legacy-result") {
                legacy_address = std::stoul(value, nullptr, 16);
                if (legacy_address < 0 || legacy_address > 0x7ff) throw std::runtime_error("legacy result must be in CPU RAM");
                test = true;
            }
            else if (arg == "--console-crc") {
                if (value.size() != 8 || value.find_first_not_of("0123456789ABCDEF") != std::string::npos)
                    throw std::runtime_error("console CRC must be 8 uppercase hexadecimal digits");
                console_crcs.push_back(value); console_test = true; test = true;
            }
            else if (arg == "--ppm") ppm_name = value;
            else if (arg == "--wav") wav_name = value;
            else if (arg == "--input") input_name = value;
            else if (arg == "--save") save_name = value;
            else if (arg == "--trace") trace_name = value;
            else throw std::runtime_error("unknown argument " + arg);
        }
        Cartridge cart(argv[1]);
        if (!save_name.empty()) {
            if (!cart.battery) throw std::runtime_error("--save requires a battery-backed cartridge");
            std::ifstream f(save_name, std::ios::binary);
            if (f) {
                auto b = read_file(save_name);
                if (b.size() != cart.ram.size()) throw std::runtime_error("save must contain exactly 8192 bytes");
                std::copy(b.begin(), b.end(), cart.ram.begin());
            }
        }
        std::vector<Input> input;
        if (!input_name.empty()) {
            std::ifstream f(input_name); if (!f) throw std::runtime_error("cannot open input script");
            std::string line;
            while (std::getline(f, line)) {
                if (line.empty() || line[0] == '#') continue;
                Input event{}; std::istringstream s(line);
                if (!(s >> std::dec >> event.frame >> std::hex >> event.p1 >> event.p2) || event.p1 > 255 || event.p2 > 255 ||
                    (!input.empty() && event.frame < input.back().frame)) throw std::runtime_error("invalid input script");
                input.push_back(event);
            }
        }
        std::ofstream trace;
        if (!trace_name.empty()) { trace.open(trace_name); if (!trace) throw std::runtime_error("cannot open trace"); trace << "cycle,address,write,data,fetch,pc,a,x,y,s,p,line,dot,irq,ppu_address\n"; }
        auto model = std::make_unique<Vnes>();
        auto &nes = *model;
        nes.mapper = cart.mapper; nes.prg_mask = cart.prg.size() / 8192 - 1; nes.chr_mask = cart.chr.size() / 1024 - 1;
        nes.vertical = cart.vertical; nes.four_screen = cart.four; nes.chr_ram = cart.chr_ram;
        nes.pad1 = 0; nes.pad2 = 0; nes.reset = 1; nes.clk = 0; nes.eval(); nes.clk = 1; nes.eval(); nes.reset = 0;
        std::array<uint32_t, 256 * 240> pixels{};
        std::array<uint8_t, 4096> console{};
        console.fill(' ');
        std::vector<int16_t> audio;
        uint64_t cycles = 0, dots = 0, pixel_count = 0;
        unsigned frames = 0, audio_phase = 0;
        size_t input_index = 0;
        bool test_started = false;
        uint64_t reset_requested = 0;
        unsigned reset_dots = 0;
        bool reset_armed = true;
        unsigned self_jumps = 0;
        int result = test ? 2 : 0;
        double previous_mix = 0, hp90 = 0, hp440 = 0, previous_hp90 = 0, lp = 0;
        // Clock-derived bounds catch hangs even if the frame output breaks.
        while (frames < max_frames && dots < uint64_t(max_frames + 2) * 341 * 262) {
            bool console_changed = false;
            nes.reset = reset_dots != 0;
            if (reset_dots) --reset_dots;
            while (input_index < input.size() && input[input_index].frame <= frames) {
                nes.pad1 = input[input_index].p1; nes.pad2 = input[input_index].p2; ++input_index;
            }
            nes.clk = 0;
            // The preceding rising-edge evaluation already presents this
            // dot's addresses. Settle memory data with the falling edge,
            // without an extra evaluation before driving the read ports.
            for (int settle = 0; settle < 4; ++settle) {
                const unsigned pa = nes.prg_address, ca = nes.chr_address, ra = nes.ram_address;
                if (pa >= cart.prg.size() || ca >= cart.chr.size() || ra >= cart.ram.size())
                    throw std::runtime_error("RTL cartridge address out of range");
                nes.prg_data = cart.prg[pa]; nes.chr_data = cart.chr[ca]; nes.ram_data = cart.ram[ra]; nes.eval();
                if (pa == nes.prg_address && ca == nes.chr_address && ra == nes.ram_address) break;
                if (settle == 3) throw std::runtime_error("external bus failed to settle");
            }
            if (nes.pixel_valid) {
                uint32_t color = palette[nes.pixel];
                if (nes.emphasis) {
                    unsigned r = (color >> 16) & 255, g = (color >> 8) & 255, b = color & 255;
                    if (nes.emphasis & 1) { g = g * 3 / 4; b = b * 3 / 4; }
                    if (nes.emphasis & 2) { r = r * 3 / 4; b = b * 3 / 4; }
                    if (nes.emphasis & 4) { r = r * 3 / 4; g = g * 3 / 4; }
                    color = (r << 16) | (g << 8) | b;
                }
                pixels[nes.pixel_y * 256 + nes.pixel_x] = color; ++pixel_count;
            }
            if (nes.frame) ++frames;
            if (nes.audio_valid && !wav_name.empty()) {
                const double pulses = nes.audio_pulse1 + nes.audio_pulse2;
                const double tnd = nes.audio_triangle / 8227.0 + nes.audio_noise / 12241.0 + nes.audio_dmc / 22638.0;
                const double mix = (pulses ? 95.88 / (8128.0 / pulses + 100) : 0) + (tnd ? 159.79 / (1.0 / tnd + 100) : 0);
                // Sample the analog approximation at 48 kHz; preserve DC DAC state in RTL.
                audio_phase += 48000 * 3;
                if (audio_phase >= 5369318) {
                    audio_phase -= 5369318;
                    hp90 = 0.9883 * (hp90 + mix - previous_mix); previous_mix = mix;
                    hp440 = 0.9440 * (hp440 + hp90 - previous_hp90); previous_hp90 = hp90;
                    lp += 0.6472 * (hp440 - lp);
                    audio.push_back(int16_t(std::clamp(lp * 30000.0, -32768.0, 32767.0)));
                }
            }
            if (nes.debug_cpu_tick) {
                if (console_test && nes.debug_write && (nes.debug_address & 0xe007) == 0x2007 &&
                    nes.debug_ppu_address >= 0x2000 && nes.debug_ppu_address < 0x3f00) {
                    console[nes.debug_ppu_address & 0xfff] = nes.debug_data;
                    console_changed = true;
                }
                if (legacy_address >= 0 && nes.debug_write && (nes.debug_address & 0xf800) == 0 &&
                    (nes.debug_address & 0x7ff) == unsigned(legacy_address)) legacy_code = nes.debug_data;
                if (legacy_address >= 0 && nes.debug_fetch && nes.debug_pc >= 0x8000) {
                    const size_t pa = nes.prg_address;
                    const bool self_jump = pa + 2 < cart.prg.size() && cart.prg[pa] == 0x4c &&
                        (cart.prg[pa + 1] | (cart.prg[pa + 2] << 8)) == nes.debug_pc;
                    self_jumps = self_jump ? self_jumps + 1 : 0;
                }
                if (trace) trace << std::dec << cycles << ',' << std::hex << nes.debug_address << ',' << unsigned(nes.debug_write) << ','
                    << unsigned(nes.debug_data) << ',' << unsigned(nes.debug_fetch) << ',' << nes.debug_pc << ',' << unsigned(nes.debug_a) << ','
                    << unsigned(nes.debug_x) << ',' << unsigned(nes.debug_y) << ',' << unsigned(nes.debug_s) << ',' << unsigned(nes.debug_p) << ','
                    << std::dec << nes.debug_line << ',' << nes.debug_dot << ',' << unsigned(nes.debug_irq) << ',' << std::hex << nes.debug_ppu_address << '\n';
                ++cycles;
            }
            if (nes.ram_write) cart.ram[nes.ram_address] = nes.ram_write_data;
            if (nes.chr_write) cart.chr[nes.chr_address] = nes.chr_write_data;
            nes.clk = 1; nes.eval(); ++dots;
            if (nes.halted) throw std::runtime_error("CPU executed a KIL opcode");
            if (console_changed) {
                std::string text;
                for (unsigned page = 0; page < 4; ++page) {
                    for (unsigned row = 0; row < 30; ++row) {
                        for (unsigned col = 0; col < 32; ++col) {
                            const uint8_t c = console[page * 1024 + row * 32 + col];
                            text.push_back(c >= 32 && c < 127 ? char(c) : ' ');
                        }
                        text.push_back('\n');
                    }
                }
                const bool passed = text.find("Passed") != std::string::npos ||
                    std::any_of(console_crcs.begin(), console_crcs.end(), [&](const std::string &crc) { return text.find(crc) != std::string::npos; });
                const bool failed = text.find("Failed") != std::string::npos;
                if (passed || failed) {
                    result = failed ? 1 : 0;
                    std::cout << (result ? "FAIL " : "PASS ") << argv[1] << " ASCII console result\n";
                    std::istringstream lines(text);
                    std::string line;
                    while (std::getline(lines, line)) {
                        const auto end = line.find_last_not_of(' ');
                        if (end != std::string::npos) std::cout << line.substr(0, end + 1) << '\n';
                    }
                    break;
                }
            }
            if (legacy_address >= 0 && self_jumps >= 3 && legacy_code >= 0) {
                result = legacy_code == 1 ? 0 : 1;
                std::cout << (result ? "FAIL " : "PASS ") << argv[1] << " legacy code=" << legacy_code << '\n';
                break;
            }
            if (test && cart.ram[1] == 0xde && cart.ram[2] == 0xb0 && cart.ram[3] == 0x61) {
                if (cart.ram[0] == 0x80) test_started = true;
                if (cart.ram[0] != 0x81 && !reset_dots) reset_armed = true;
                if (cart.ram[0] == 0x81 && !reset_dots && reset_armed) {
                    if (!reset_requested) reset_requested = dots;
                    if (dots - reset_requested >= 536932) {
                        reset_dots = 9; reset_requested = 0; reset_armed = false;
                    }
                }
                if (test_started && cart.ram[0] < 0x80) {
                    result = cart.ram[0] ? 1 : 0;
                    std::cout << (result ? "FAIL " : "PASS ") << argv[1] << " code=" << unsigned(cart.ram[0]) << '\n';
                    for (size_t i = 4; i < cart.ram.size() && cart.ram[i]; ++i) std::cout << char(cart.ram[i]);
                    std::cout << '\n'; break;
                }
            }
        }
        if (test && result == 2) std::cerr << "TIMEOUT " << argv[1] << " PC=" << std::hex << nes.debug_pc << std::dec << '\n';
        if (!ppm_name.empty()) {
            std::ofstream f(ppm_name, std::ios::binary); if (!f) throw std::runtime_error("cannot write PPM");
            f << "P6\n256 240\n255\n";
            for (auto p : pixels) { f.put((p >> 16) & 255); f.put((p >> 8) & 255); f.put(p & 255); }
        }
        if (!wav_name.empty()) wav(wav_name, audio);
        if (!save_name.empty()) {
            std::ofstream f(save_name, std::ios::binary); if (!f) throw std::runtime_error("cannot write save");
            f.write(reinterpret_cast<const char *>(cart.ram.data()), cart.ram.size());
        }
        std::cout << "mapper=" << unsigned(cart.mapper) << " frames=" << frames << " cpu_cycles=" << cycles << " pixels=" << pixel_count << '\n';
        nes.final();
        return result;
    } catch (const std::exception &e) { std::cerr << "nes_sim: " << e.what() << '\n'; return 1; }
}
