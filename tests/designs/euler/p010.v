// Project Euler 10: the sum of the primes below two million.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P010.lean).
//
// A segmented sieve of Eratosthenes over the odd numbers, in 17 segments of
// 61440, spread over fifteen engines, each with its own block of 4096 odd
// numbers (one block RAM, 256 words of 16 bits) and its own block RAM of
// per-prime state: the offset of the next odd multiple to strike in its block.
// * Setup: trial division by the primes up to 37 lists the 222 odd primes below
//   sqrt(2 10^6); for each prime p, 4096 mod p comes from twelve modular
//   doublings, and the engines' first offsets follow by repeated modular
//   subtraction, one engine per cycle.
// * Sieving, in lockstep: all engines take the same prime at once; for
//   p >= 17 every cycle strikes one bit (a read-modify-write), and the offset
//   for the next segment is where the walk leaves the block, moved on by
//   57344 mod p with one modular subtraction; for p <= 13 every cycle ors a
//   whole word with a mask from a small table.
// * Summing: three read-back units, each serving five neighbouring engines,
//   read the words back (clearing them for the next segment) and accumulate,
//   with additions only, the count of primes C, the running sum of counts R
//   and the in-word positions; a combiner folds each unit into the total.
module euler10 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [37:0] answer
);
  localparam E = 15, W = 38, SCANNERS = 3, PER_SCANNER = 5, BANK_WORDS = 256;
  localparam SCAN_WORDS = PER_SCANNER * BANK_WORDS;   // 1280
  localparam SEGMENTS = 17, WORD_LIMIT = 62500, MAX_CANDIDATE = 1413;
  localparam C_PRE = 3'd0, C_LOAD = 3'd1, C_MARK = 3'd2, C_SCAN_GO = 3'd3, C_SCAN = 3'd4,
    C_COMB = 3'd5, C_FINAL = 3'd6, C_DONE = 3'd7;
  localparam P_SCAN = 2'd0, P_MOD = 2'd1, P_CHAIN = 2'd2, P_FIN = 2'd3;

  function integer trial_prime(input integer i);
    case (i)
      0: trial_prime = 3; 1: trial_prime = 5; 2: trial_prime = 7; 3: trial_prime = 11;
      4: trial_prime = 13; 5: trial_prime = 17; 6: trial_prime = 19; 7: trial_prime = 23;
      8: trial_prime = 29; 9: trial_prime = 31; default: trial_prime = 37;
    endcase
  endfunction

  // the strike pattern of small prime k3 (bits 0, p, 2p, ...), 1 otherwise
  function [15:0] small_pat(input [2:0] k3);
    case (k3)
      0: small_pat = 16'b1001001001001001;
      1: small_pat = 16'b1000010000100001;
      2: small_pat = 16'b0100000010000001;
      3: small_pat = 16'b0000100000000001;
      4: small_pat = 16'b0010000000000001;
      default: small_pat = 16'd1;
    endcase
  endfunction

  // popcount and position sum (sum of t z_t) of a 16-bit word
  function [11:0] word_stats(input [15:0] z);
    reg [4:0] cnt;
    reg [6:0] pos;
    integer t;
    begin
      cnt = 0;
      pos = 0;
      for (t = 0; t < 16; t = t + 1)
        if (z[t]) begin
          cnt = cnt + 5'd1;
          pos = pos + t;
        end
      word_stats = {pos, cnt};
    end
  endfunction

  // controller
  reg [2:0] ph;
  reg [7:0] k, kmax;
  reg [4:0] segn;
  reg [15:0] segw;
  reg [2:0] ce;
  reg [4:0] cs;
  reg [W-1:0] v0, sumr, mb, total;
  reg [16:0] ma;
  // set-up
  reg [1:0] pst;
  reg [10:0] m, pc, prr;
  reg [3:0] pcnt, pb;
  reg [7:0] kk;
  reg [5:0] small_sum;
  reg [7:0] wsm;
  reg idm, ids;
  reg [21:0] pkd, pkd2;
  reg [5:0] trs [0:10];
  reg [21:0] primes [0:255];

  wire is_pre = ph == C_PRE;
  wire is_load = ph == C_LOAD;
  wire is_mark = ph == C_MARK;
  wire is_scan_go = ph == C_SCAN_GO;
  wire is_scan = ph == C_SCAN;
  wire is_comb = ph == C_COMB;
  wire is_final = ph == C_FINAL;

  // set-up: candidates by trial division
  wire is_pscan = is_pre && pst == P_SCAN;
  wire is_pmod = is_pre && pst == P_MOD;
  wire is_pchain = is_pre && pst == P_CHAIN;
  wire is_pfin = is_pre && pst == P_FIN;
  reg composite;
  integer i;
  always @* begin
    composite = 0;
    for (i = 0; i < 11; i = i + 1)
      if (trs[i] == 0 && m != trial_prime(i)) composite = 1;
  end
  wire past = MAX_CANDIDATE < m;
  wire found_p = is_pscan && !past && !composite;
  wire skip_m = is_pscan && !past && composite;
  wire mod_last = pcnt == 11;
  wire chain_last = pb == E - 1;
  wire advance_m = skip_m || (is_pchain && chain_last);
  // 4096 mod p: twelve modular doublings
  wire [11:0] d2 = {pc, 1'b0};
  wire [11:0] m12 = {1'b0, m};
  wire [11:0] pc_dbl = d2 < m12 ? d2 : d2 - m12;
  // the chain: offsets ((p - 1)/2 - 4096 b) mod p for engines b = 0 ... 14
  wire [10:0] half = m >> 1;
  wire [10:0] chain_sub = prr - pc;
  wire [10:0] chain_next = prr < pc ? chain_sub + m : chain_sub;
  wire [11:0] bank0 = m < 17 ? {1'b0, half} : {1'b0, half} + m12;
  wire is_b0 = pb == 0;
  wire [11:0] chain_val = is_b0 ? bank0 : {1'b0, chain_next};
  wire setup_done = is_pfin;
  // the prime list: the prime and 57344 mod p = ((p - 1)/2 - r_14) mod p
  wire [10:0] pk = pkd[10:0];
  wire [10:0] pk2 = pkd2[10:0];
  wire [10:0] dk = pkd2[21:11];
  wire [10:0] d_sub = half - chain_next;
  wire [10:0] d_val = half < chain_next ? d_sub + m : d_sub;
  wire prime_write = is_pchain && chain_last;

  // engines, in lockstep
  wire go = is_load;
  wire is_small_k = k < 5;
  wire [2:0] k3 = k[2:0];
  wire [15:0] pat = is_small_k ? small_pat(k3) : 16'd1;
  wire wsm_last = wsm == BANK_WORDS - 1;
  wire [E-1:0] eng_idle, scan_valid;
  wire [15:0] eng_data [0:E-1];
  wire [10:0] sw [0:SCANNERS-1];

  genvar e, u;
  generate
    for (e = 0; e < E; e = e + 1) begin : engine
      wire [10:0] s = sw[e / PER_SCANNER];
      wire scan_en = s < SCAN_WORDS && s[10:8] == e % PER_SCANNER;
      euler10_engine eng (
        .clk(clk), .rst(rst), .go(go), .scan_go(is_scan_go), .is_small(is_small_k),
        .pk(pk), .pk2(pk2), .dk(dk), .k(k), .scan_en(scan_en), .scan_addr(s[7:0]),
        .setup_write(is_pchain && pb == e), .setup_addr(kk), .setup_val(chain_val),
        .pat(pat), .k3(k3), .wsm(wsm), .wsm_last(wsm_last),
        .idle(eng_idle[e]), .data(eng_data[e]), .scan_valid(scan_valid[e]));
    end
  endgenerate

  // read-back units: a word a cycle from their engines, in order; the word's
  // statistics a cycle after its read
  wire [SCANNERS-1:0] scan_busy;
  wire [14:0] cc [0:SCANNERS-1];
  wire [24:0] rr [0:SCANNERS-1];
  wire [17:0] pp [0:SCANNERS-1];
  generate
    for (u = 0; u < SCANNERS; u = u + 1) begin : unit
      reg [10:0] swr;
      reg sl, zv, sv;
      reg [14:0] ccr = 0;
      reg [24:0] rrr = 0;
      reg [17:0] ppr = 0;
      reg [15:0] zr = 0;
      reg [4:0] cn = 0;
      reg [6:0] po = 0;
      assign sw[u] = swr;
      assign cc[u] = ccr;
      assign rr[u] = rrr;
      assign pp[u] = ppr;
      wire reading = swr < SCAN_WORDS;
      // below 2 10^6: word index segw + 1280u + sw < 62500
      wire [16:0] word = {1'b0, segw} + SCAN_WORDS * u + {6'd0, swr};
      wire below = word < WORD_LIMIT;
      reg [15:0] z;
      integer j;
      always @* begin
        z = 0;
        for (j = 0; j < PER_SCANNER; j = j + 1)
          if (scan_valid[PER_SCANNER * u + j]) z = z | ~eng_data[PER_SCANNER * u + j];
      end
      wire valid = |scan_valid[PER_SCANNER * u +: PER_SCANNER];
      wire [11:0] stats = word_stats(zr);
      assign scan_busy[u] = reading || valid || zv || sv;
      always @(posedge clk) begin
        if (rst) begin
          swr <= 0;
          sl <= 0;
          zv <= 0;
          sv <= 0;
        end else begin
          sl <= reading && below;
          if (is_scan_go) swr <= 0;
          else if (reading) swr <= swr + 11'd1;
          zv <= valid;
          sv <= zv;
        end
        zr <= sl ? z : 16'd0;
        cn <= stats[4:0];
        po <= stats[11:5];
        if (is_scan_go) begin
          ccr <= 0;
          rrr <= 0;
          ppr <= 0;
        end else if (sv) begin
          ccr <= ccr + cn;
          rrr <= rrr + ccr;
          ppr <= ppr + po;
        end
      end
    end
  endgenerate

  wire all_idle = &eng_idle;
  wire scans_idle = !(|scan_busy);
  wire last_prime = k + 8'd1 == kmax;
  wire mark_end = is_mark && idm;

  // combining, one addition a cycle: per read-back unit u the sum of its primes
  // is C V0_u + 32 (1280 C - R) + 2 positions. Steps 0-2 add 40960 C (as
  // 2^15 C + 2^13 C) and 2 positions; step 3 starts C V0_u (fifteen serial
  // steps, 4 ... 18); steps 19 and 20 collect R (its 32 R is subtracted once,
  // at the end), and step 20 moves V0 on by 40960.
  wire [1:0] ce_i = ce[1] ? 2'd2 : ce[1:0];
  wire [W-1:0] cc_u = {{(W-15){1'b0}}, cc[ce_i]};
  wire [W-1:0] rr_u = {{(W-25){1'b0}}, rr[ce_i]};
  wire [W-1:0] pp_u = {{(W-18){1'b0}}, pp[ce_i]};
  wire multiplying = is_comb && cs >= 4 && cs < 19;
  wire [W-1:0] operand = (is_comb && cs == 0 ? cc_u << 15 : {W{1'b0}})
    | (is_comb && cs == 1 ? cc_u << 13 : {W{1'b0}})
    | (is_comb && cs == 2 ? pp_u << 1 : {W{1'b0}})
    | (multiplying && ma[0] ? mb : {W{1'b0}});
  wire unit_end = is_comb && cs == 20;
  wire collect_q = is_comb && cs == 19;
  wire comb_last = unit_end && ce == SCANNERS - 1;
  wire start_mul = is_comb && cs == 3;
  wire last_seg = segn == SEGMENTS - 1;
  wire to_next = comb_last && !last_seg;
  wire to_final = comb_last && last_seg;
  wire finished = is_final && !done;

  always @(posedge clk) begin
    if (prime_write) primes[kk] <= {d_val, m};
    if (rst) begin
      ph <= C_PRE;
      k <= 0;
      kmax <= 0;
      segn <= 0;
      segw <= 0;
      ce <= 0;
      cs <= 0;
      v0 <= 1;
      sumr <= 0;
      ma <= 0;
      mb <= 0;
      total <= 0;
      done <= 0;
      answer <= 0;
      pst <= P_SCAN;
      m <= 3;
      pc <= 1;
      pcnt <= 0;
      pb <= 0;
      prr <= 0;
      kk <= 0;
      small_sum <= 0;
      wsm <= 0;
      idm <= 0;
      ids <= 0;
      pkd <= 0;
      pkd2 <= 0;
      for (i = 0; i < 11; i = i + 1) trs[i] <= 3 % trial_prime(i);
    end else begin
      // set-up
      if (advance_m) m <= m + 11'd2;
      for (i = 0; i < 11; i = i + 1)
        if (advance_m) trs[i] <= trs[i] + 6'd2 >= trial_prime(i) ? trs[i] + 6'd2 - trial_prime(i) : trs[i] + 6'd2;
      if (found_p) pc <= 1;
      else if (is_pmod) pc <= pc_dbl[10:0];
      if (found_p) pcnt <= 0;
      else if (is_pmod) pcnt <= pcnt + 4'd1;
      if (is_pmod && mod_last) prr <= half;
      else if (is_pchain && !is_b0) prr <= chain_next;
      if (is_pmod && mod_last) pb <= 0;
      else if (is_pchain) pb <= pb + 4'd1;
      if (is_pchain && chain_last) kk <= kk + 8'd1;
      if (is_pchain && is_b0 && m < 17) small_sum <= small_sum + m[5:0];
      if (found_p) pst <= P_MOD;
      else if (is_pmod && mod_last) pst <= P_CHAIN;
      else if (is_pchain && chain_last) pst <= P_SCAN;
      else if (is_pscan && past) pst <= P_FIN;
      pkd <= primes[k];
      pkd2 <= pkd;
      wsm <= go ? 8'd0 : wsm + 8'd1;
      idm <= is_mark && all_idle;
      ids <= is_scan && all_idle && scans_idle;
      // combining
      if (start_mul) ma <= {2'b00, cc_u[14:0]};
      else if (multiplying) ma <= ma >> 1;
      if (start_mul) mb <= v0;
      else if (multiplying) mb <= {mb[W-2:0], 1'b0};
      cs <= is_comb ? (unit_end ? 5'd0 : cs + 5'd1) : 5'd0;
      ce <= is_comb ? (unit_end ? ce + 3'd1 : ce) : 3'd0;
      if (is_scan_go) v0 <= {{(W-21){1'b0}}, segw, 5'd0} + 1'b1;
      else if (unit_end) v0 <= v0 + 32 * SCAN_WORDS;
      if (collect_q) sumr <= sumr + rr_u;
      else if (unit_end) sumr <= sumr + cc_u;
      // the small primes (struck from themselves) and 1 (never struck): + small_sum - 1 + 2
      if (setup_done) total <= total + small_sum + 1'b1;
      else if (is_comb) total <= total + operand;
      // phases
      if (setup_done) ph <= C_LOAD;
      else if (is_load) ph <= C_MARK;
      else if (mark_end) ph <= last_prime ? C_SCAN_GO : C_LOAD;
      else if (is_scan_go) ph <= C_SCAN;
      else if (is_scan && ids) ph <= C_COMB;
      else if (to_next) ph <= C_LOAD;
      else if (to_final) ph <= C_FINAL;
      else if (finished) ph <= C_DONE;
      if (mark_end) k <= last_prime ? 8'd0 : k + 8'd1;
      if (setup_done) kmax <= kk;
      if (to_next) begin
        segn <= segn + 5'd1;
        segw <= segw + 16'd3840;
      end
      // the end: total - 32 sum R
      if (finished) answer <= total - {sumr[W-6:0], 5'd0};
      done <= done || finished;
    end
  end
endmodule

// One sieve engine: a block of 4096 odd numbers and its per-prime offsets.
// `go` starts prime k (offset from its table); `scan_go` starts reading back,
// a word whenever `scan_en` (at `scan_addr`). Strikes are masks: the prime's
// pattern (one bit for p >= 17, bits 0, p, 2p, ... below) shifted to the
// offset within the word.
module euler10_engine (
  input wire clk,
  input wire rst,
  input wire go,
  input wire scan_go,
  input wire is_small,
  input wire [10:0] pk,
  input wire [10:0] pk2,
  input wire [10:0] dk,
  input wire [7:0] k,
  input wire scan_en,
  input wire [7:0] scan_addr,
  input wire setup_write,
  input wire [7:0] setup_addr,
  input wire [11:0] setup_val,
  input wire [15:0] pat,
  input wire [2:0] k3,
  input wire [7:0] wsm,
  input wire wsm_last,
  output wire idle,
  output wire [15:0] data,
  output wire scan_valid
);
  localparam E_IDLE = 2'd0, E_LARGE = 2'd1, E_SMALL = 2'd2, E_SCAN = 2'd3;
  localparam BANK = 4096;

  // (rho - 16) mod p for the small prime k3
  function [3:0] rho_next_of(input [2:0] k3, input [3:0] rho);
    case (k3)
      0: rho_next_of = (rho + 16 * 3 - 16) % 3;
      1: rho_next_of = (rho + 16 * 5 - 16) % 5;
      2: rho_next_of = (rho + 16 * 7 - 16) % 7;
      3: rho_next_of = (rho + 16 * 11 - 16) % 11;
      4: rho_next_of = (rho + 16 * 13 - 16) % 13;
      default: rho_next_of = 0;
    endcase
  endfunction

  reg [15:0] bits [0:255];
  reg [11:0] offs [0:255];
  reg [1:0] st;
  reg [12:0] idx = 0;
  reg [3:0] rho = 0;
  reg [7:0] raddr = 0;
  reg pv;
  reg [15:0] pm = 0;
  reg ps = 0;

  wire is_large = st == E_LARGE;
  wire is_small_st = st == E_SMALL;
  wire is_scan = st == E_SCAN;
  wire [11:0] offset = offs[k];
  assign data = bits[raddr];
  // large primes: strike bit idx while inside the block
  wire inside = idx < BANK;
  wire strike = is_large && inside;
  wire leave = is_large && !inside;
  wire [3:0] rho_next = rho_next_of(k3, rho);
  wire small_last = is_small_st && wsm_last;
  wire scan_read = is_scan && scan_en;
  wire scan_last = scan_read && scan_addr == 8'd255;
  wire issue = strike || is_small_st || scan_read;
  wire [7:0] addr = is_large ? idx[11:4] : is_scan ? scan_addr : wsm;
  wire [3:0] amount = is_large ? idx[3:0] : rho;
  // the pending write: or the mask in, or (reading back) clear the word
  wire [15:0] wdata = ps ? 16'd0 : data | pm;
  wire to_idle = leave || small_last || scan_last;
  // the offset in this engine's block of the next segment, 14 blocks further
  // on: from where the walk leaves the block (or the last rho), minus 57344 mod p
  wire [10:0] after = leave ? idx[10:0] : {7'd0, rho_next};
  wire [11:0] back = {1'b0, after} - {1'b0, dk};
  wire [11:0] new_off = back[11] ? back + {1'b0, pk2} : back;
  wire off_write = leave || small_last;
  assign idle = st == E_IDLE && !pv;
  assign scan_valid = pv && ps;

  always @(posedge clk) begin
    if (pv) bits[raddr] <= wdata;
    if (off_write || setup_write) offs[setup_write ? setup_addr : k] <= setup_write ? setup_val : new_off;
    if (go) idx <= {1'b0, offset};
    else if (strike) idx <= idx + {2'b00, pk};
    if (go) rho <= offset[3:0];
    else if (is_small_st) rho <= rho_next;
    raddr <= addr;
    pm <= pat << amount;
    ps <= is_scan;
    if (rst) begin
      st <= E_SCAN;
      pv <= 0;
    end else begin
      if (go) st <= is_small ? E_SMALL : E_LARGE;
      else if (scan_go) st <= E_SCAN;
      else if (to_idle) st <= E_IDLE;
      pv <= issue;
    end
  end
endmodule
