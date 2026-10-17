// Project Euler 43: the sum of the 0-9 pandigital numbers d1 d2 ... d10 in
// which d2d3d4 is divisible by 2, d3d4d5 by 3, d4d5d6 by 5, d5d6d7 by 7,
// d6d7d8 by 11, d7d8d9 by 13 and d8d9d10 by 17.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P043.lean).
//
// A depth-first search from the right, one digit per cycle, pruned as soon as
// a three-digit window fails. Each level keeps the residue -(10a + b) mod p
// that the window c a b needs from 100c, so testing a candidate is one table
// lookup and a compare. The digit registers are the search stack, a ten-bit
// mask holds the digits in use. Ten lanes take the ten choices of d10. A
// completed number is turned into binary in ten steps (v <- 10v + d) and added
// to the lane's sum; the lanes' sums are added at the end.
module euler43 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [35:0] answer
);
  localparam W = 36, LANES = 10;

  reg [3:0] scan;
  reg [W-1:0] sum;
  wire [LANES-1:0] lane_done;
  wire [W-1:0] tot [0:LANES-1];

  genvar l;
  generate
    for (l = 0; l < LANES; l = l + 1) begin : lane
      euler43_lane #(.L(l)) u (.clk(clk), .rst(rst), .done(lane_done[l]), .tot(tot[l]));
    end
  endgenerate

  wire all_done = &lane_done;
  wire scanning = all_done && scan < LANES;
  wire finished = all_done && scan == LANES && !done;

  always @(posedge clk)
    if (rst) begin
      scan <= 0;
      sum <= 0;
      done <= 0;
      answer <= 0;
    end else begin
      if (scanning) begin
        sum <= sum + tot[scan];
        scan <= scan + 4'd1;
      end
      if (finished) answer <= sum;
      done <= done || finished;
    end
endmodule

// One lane: the numbers ending in digit L.
module euler43_lane #(
  parameter L = 0
) (
  input wire clk,
  input wire rst,
  output wire done,
  output reg [35:0] tot
);
  localparam W = 36;
  localparam M_SEARCH = 2'd0, M_CONV = 2'd1, M_ADD = 2'd2, M_DONE = 2'd3;

  // the prime of the window starting at position i (2 ... 8)
  function integer prime_of(input integer i);
    case (i)
      2: prime_of = 2; 3: prime_of = 3; 4: prime_of = 5; 5: prime_of = 7;
      6: prime_of = 11; 7: prime_of = 13; 8: prime_of = 17; default: prime_of = 1;
    endcase
  endfunction

  // Read-only tables over {i, x} (position, digit), as in HDLean: the prime,
  // (10 x) mod p, x mod p and (100 x) mod p for the window at position i.
  function [5*256-1:0] table_of(input integer k);
    integer x;
    begin
      for (x = 0; x < 256; x = x + 1)
        case (k)
          0: table_of[5*x +: 5] = prime_of(x % 16);
          1: table_of[5*x +: 5] = (10 * (x % 16)) % prime_of(x / 16);
          2: table_of[5*x +: 5] = (x % 16) % prime_of(x / 16);
          default: table_of[5*x +: 5] = (100 * (x % 16)) % prime_of(x / 16);
        endcase
    end
  endfunction
  localparam [5*256-1:0] PRIME = table_of(0), R10 = table_of(1), R1 = table_of(2), R100 = table_of(3);

  // the residue 100 c must have modulo the prime of the window at position i
  // for the window c a b to be divisible: -(10a + b) mod p
  function [4:0] need_of(input [3:0] i, input [3:0] a, input [3:0] b);
    reg [4:0] p, r10, r1, s1;
    reg [5:0] s;
    begin
      p = PRIME[5 * i +: 5];
      r10 = R10[5 * {i, a} +: 5];
      r1 = R1[5 * {i, b} +: 5];
      s = r10 + r1;
      s1 = s < p ? s : s - p;
      need_of = s1 == 0 ? 5'd0 : p - s1;
    end
  endfunction

  // d_i (index i - 1 into d1 ... d10), as HDLean's multiplexer tree picks it:
  // indices from 8 up only look at bit 0
  function [3:0] digit_at(input [3:0] i, input [39:0] ds);
    reg [3:0] idx;
    begin
      idx = i - 4'd1;
      digit_at = idx[3] ? ds[4 * (8 + idx[0]) +: 4] : ds[4 * idx[2:0] +: 4];
    end
  endfunction

  reg [3:0] d [1:9];
  reg [9:0] used;
  reg [3:0] pos, cand, k, wa;
  reg [1:0] mode;
  reg [W-1:0] v;
  reg [4:0] need;
  reg win;

  wire [39:0] digits = {L[3:0], d[9], d[8], d[7], d[6], d[5], d[4], d[3], d[2], d[1]};
  wire is_search = mode == M_SEARCH;
  wire is_conv = mode == M_CONV;
  wire is_add = mode == M_ADD;
  assign done = mode == M_DONE;

  wire [4:0] rc = R100[5 * {pos, cand} +: 5];
  wire div_ok = !win || rc == need;
  wire cand_bit = cand < 10 ? used[cand] : 1'b0;
  wire exhausted = cand == 10;
  wire ok = !exhausted && !cand_bit && div_ok;
  wire at_one = pos == 1;
  wire at_top = pos == 9;
  wire backtrack = is_search && exhausted;
  wire finish_lane = backtrack && at_top;
  wire up = backtrack && !at_top;
  wire place = is_search && ok;
  wire descend = place && !at_one;
  wire complete = place && at_one;
  wire skip_c = is_search && !exhausted && !ok;
  wire [9:0] prev_mask = 10'd1 << wa;
  wire [9:0] cand_mask = 10'd1 << cand;
  wire [3:0] pos_dn = pos - 4'd1;
  wire [3:0] pos_up = pos + 4'd1;
  wire [3:0] a2 = digit_at(pos + 4'd2, digits);
  wire [3:0] a3 = digit_at(pos + 4'd3, digits);
  // one table set per lane: going up needs `exhausted`, going down excludes it
  wire [3:0] npos = exhausted ? pos_up : pos_dn;
  wire [4:0] n_need = need_of(npos, exhausted ? a2 : cand, exhausted ? a3 : wa);
  wire n_win = npos >= 2 && npos < 9;
  wire moved = descend || up;
  wire [3:0] dk = digit_at(k, digits);
  wire conv_last = k == 10;

  integer i;
  always @(posedge clk)
    if (rst) begin
      for (i = 1; i <= 9; i = i + 1) d[i] <= 0;
      used <= 10'd1 << L;
      pos <= 9;
      cand <= 0;
      mode <= M_SEARCH;
      k <= 0;
      v <= 0;
      tot <= 0;
      wa <= L;
      need <= 0;
      win <= 0;
    end else begin
      if (descend) used <= used | cand_mask;
      else if (up) used <= used & ~prev_mask;
      if (descend) pos <= pos_dn;
      else if (up) pos <= pos_up;
      if (descend) cand <= 0;
      else if (up) cand <= wa + 4'd1;
      else if (skip_c || is_add) cand <= cand + 4'd1;
      for (i = 1; i <= 9; i = i + 1)
        if (place && pos == i) d[i] <= cand;
      if (descend) wa <= cand;
      else if (up) wa <= a2;
      if (moved) begin
        need <= n_need;
        win <= n_win;
      end
      if (complete) v <= 0;
      else if (is_conv) v <= {v[W-4:0], 3'b000} + {v[W-2:0], 1'b0} + dk;
      if (complete) k <= 1;
      else if (is_conv) k <= k + 4'd1;
      if (is_add) tot <= tot + v;
      if (complete) mode <= M_CONV;
      else if (is_conv && conv_last) mode <= M_ADD;
      else if (is_add) mode <= M_SEARCH;
      else if (finish_lane) mode <= M_DONE;
    end
endmodule
