// Project Euler 48: the last ten digits of 1^1 + 2^2 + ... + 1000^1000.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P048.lean).
//
// Modulo 10^10 = 2^10 * 5^10, by the Chinese remainder theorem: every term is
// computed modulo 5^10 (24-bit residues) and modulo 2^10, the two sums are
// combined once at the end. Terms n^n with 10 | n vanish and are skipped.
// Six lanes take the exponents from a shared counter. A lane runs
// left-to-right square-and-multiply: squarings modulo 5^10 on a radix-4
// interleaved multiplier (twelve digits, seven candidate reductions side by
// side), the multiplications by n on the same multiplier with only the five
// digits of n; alongside, the residue modulo 2^10 is squared and multiplied
// by a ten-bit multiplier. At the end the lanes' sums are added and combined:
// X = s5 + 5^10 ((s2 - s5) 5^-10 mod 2^10).
module euler48 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [33:0] answer
);
  localparam MW = 24, W = 34, LANES = 6;
  localparam [24:0] MOD5 = 25'd9765625;
  localparam [9:0] INV5 = 10'd841;
  localparam [9:0] LIMIT = 10'd1000;

  function [MW-1:0] add_mod(input [MW-1:0] x, input [MW-1:0] y);
    reg [MW:0] s, t;
    begin
      s = {1'b0, x} + {1'b0, y};
      t = s - MOD5;
      add_mod = t[MW] ? s[MW-1:0] : t[MW-1:0];
    end
  endfunction

  reg [9:0] nn;
  reg [3:0] u10;     // nn mod 10
  reg left;
  reg [3:0] scan;
  reg [MW-1:0] t5;
  reg [9:0] t2;
  reg [2:0] fin;
  reg [9:0] tt;
  reg [W-1:0] acc, mb;

  wire [LANES-1:0] idle;
  wire [LANES-1:0] load;
  wire [MW-1:0] s5 [0:LANES-1];
  wire [9:0] s2 [0:LANES-1];

  // exponents 1 ... 1000, skipping multiples of 10, to the lowest idle lane
  wire skip = u10 == 0;
  wire give = left && !skip && |idle;
  genvar l;
  generate
    for (l = 0; l < LANES; l = l + 1) begin : lane
      if (l == 0) begin : first
        assign load[l] = give && idle[l];
      end else begin : rest
        assign load[l] = give && idle[l] && !(|idle[l-1:0]);
      end
      euler48_lane u (.clk(clk), .rst(rst), .load(load[l]), .n_in(nn), .idle(idle[l]),
        .s5(s5[l]), .s2(s2[l]));
    end
  endgenerate

  wire advance = left && (skip || give);
  wire quiet = !left && &idle;
  wire scanning = quiet && scan < LANES;
  wire summed = quiet && scan == LANES;
  wire f0 = summed && fin == 0;
  wire f1 = fin == 1;
  wire f2 = fin == 2;
  wire [9:0] diff = t2 - t5[9:0];
  wire finished = fin == 3 && !done;

  always @(posedge clk)
    if (rst) begin
      nn <= 1;
      u10 <= 1;
      left <= 1;
      scan <= 0;
      t5 <= 0;
      t2 <= 0;
      fin <= 0;
      tt <= 0;
      acc <= 0;
      mb <= 0;
      done <= 0;
      answer <= 0;
    end else begin
      if (advance) begin
        nn <= nn + 10'd1;
        u10 <= u10 == 9 ? 4'd0 : u10 + 4'd1;
      end
      left <= left && !(advance && nn == LIMIT);
      if (scanning) begin
        t5 <= add_mod(t5, s5[scan]);
        t2 <= t2 + s2[scan];
        scan <= scan + 4'd1;
      end
      // fin: 0 summing, 1 t = (t2 - t5) 5^-10 mod 2^10, 2: X = t5 + 5^10 t
      // serially over t's bits, 3 done
      if (f0) tt <= diff * INV5;
      else if (f2) tt <= tt >> 1;
      if (f0) acc <= {{(W-MW){1'b0}}, t5};
      else if (f2 && tt[0]) acc <= acc + mb;
      if (f0) mb <= MOD5;
      else if (f2) mb <= {mb[W-2:0], 1'b0};
      if (f0) fin <= 1;
      else if (f1) fin <= 2;
      else if (f2 && tt == 0) fin <= 3;
      if (finished) answer <= acc;
      done <= done || finished;
    end
endmodule

// One lane: n^n modulo 5^10 and 2^10, added to the lane's sums.
module euler48_lane (
  input wire clk,
  input wire rst,
  input wire load,
  input wire [9:0] n_in,
  output wire idle,
  output reg [23:0] s5,
  output reg [9:0] s2
);
  localparam MW = 24, YW = 28;
  localparam [27:0] MOD5 = 28'd9765625;
  localparam S_IDLE = 3'd0, S_NEXT = 3'd1, S_PREP = 3'd2, S_MUL = 3'd3, S_AFTER = 3'd4, S_ADD = 3'd5;

  function [MW-1:0] add_mod(input [MW-1:0] x, input [MW-1:0] y);
    reg [MW:0] s, t;
    begin
      s = {1'b0, x} + {1'b0, y};
      t = s - MOD5[MW:0];
      add_mod = t[MW] ? s[MW-1:0] : t[MW-1:0];
    end
  endfunction

  // the bit length of x: the position of its top set bit, plus one
  function [3:0] bit_len(input [9:0] x);
    integer i;
    begin
      bit_len = 0;
      for (i = 0; i < 10; i = i + 1)
        if (x[i]) bit_len = i + 1;
    end
  endfunction

  reg [2:0] st;
  reg [9:0] n = 0, x2 = 0;
  reg [MW-1:0] x = 0, a = 0, acc = 0;
  reg [3:0] bi = 0, cnt = 0;
  reg isn = 0;
  reg [MW:0] b = 0;
  reg [MW+1:0] b3 = 0;
  reg [YW-1:0] y [0:6];

  wire is_idle = st == S_IDLE;
  wire is_next = st == S_NEXT;
  wire is_prep = st == S_PREP;
  wire is_mul = st == S_MUL;
  wire is_after = st == S_AFTER;
  wire is_add = st == S_ADD;
  assign idle = is_idle;

  // one radix-4 step: 4 acc + d B - k M, keep the last non-negative one
  wire [YW-1:0] acc4 = {2'b00, acc, 2'b00};
  reg [MW-1:0] red;
  reg [YW-1:0] t;
  integer k;
  always @* begin
    t = acc4 + y[0];
    red = t[MW-1:0];
    for (k = 1; k < 7; k = k + 1) begin
      t = acc4 + y[k];
      if (!t[YW-1]) red = t[MW-1:0];
    end
  end
  wire [1:0] d_next = is_prep ? a[MW-1:MW-2] : a[MW-3:MW-4];
  wire [YW-1:0] b_y = {{(YW-MW-1){1'b0}}, b};
  wire [YW-1:0] lo = d_next[0] ? b_y : {YW{1'b0}};
  wire [YW-1:0] hi = d_next[0] ? {{(YW-MW-2){1'b0}}, b3} : {b_y[YW-2:0], 1'b0};
  wire [YW-1:0] d_b = d_next[1] ? hi : lo;

  wire cnt0 = cnt == 0;
  // the exponent: bits of n below the top one, from bi down; bi = 15 when finished
  wire n_bit = bi < 10 ? n[bi] : 1'b0;
  wire finished = bi == 15;
  wire start_sq = is_next && !finished;
  wire start_n = is_after && !isn && n_bit;
  wire start = start_sq || start_n;
  // B: x, or (multiplying by n right after a squaring) the square just computed
  wire [MW-1:0] b_src = is_after ? acc : x;
  wire after_sq = is_after && !isn;
  wire leave_bit = (after_sq && !n_bit) || (is_after && isn);
  wire loading = is_idle && load;

  always @(posedge clk) begin
    if (rst) begin
      st <= S_IDLE;
      s5 <= 0;
      s2 <= 0;
    end else begin
      case (st)
        S_IDLE: if (load) st <= S_NEXT;
        S_NEXT: st <= finished ? S_ADD : S_PREP;
        S_PREP: st <= S_MUL;
        S_MUL: if (cnt0) st <= S_AFTER;
        S_AFTER: st <= start_n ? S_PREP : S_NEXT;
        S_ADD: st <= S_IDLE;
        default: ;
      endcase
      if (is_add) begin
        s5 <= add_mod(s5, x);
        s2 <= s2 + x2;
      end
    end
  end

  always @(posedge clk) begin
    if (loading) n <= n_in;
    if (loading) x <= {{(MW-10){1'b0}}, n_in};
    else if (is_after) x <= acc;
    if (loading) x2 <= n_in;
    else if (after_sq) x2 <= x2 * x2;
    else if (is_after && isn) x2 <= x2 * n;
    if (loading) bi <= bit_len(n_in) - 4'd2;
    else if (leave_bit) bi <= bi - 4'd1;
    if (start_n) isn <= 1;
    else if (start_sq) isn <= 0;
    if (start) a <= start_n ? {n, {(MW-10){1'b0}}} : x;
    else if (is_mul) a <= {a[MW-3:0], 2'b00};
    if (start) b <= {1'b0, b_src};
    if (start) b3 <= {2'b00, b_src} + {1'b0, b_src, 1'b0};
    if (start) acc <= 0;
    else if (is_mul) acc <= red;
    if (start) cnt <= start_n ? 4'd4 : 4'd11;
    else if (is_mul) cnt <= cnt - 4'd1;
    for (k = 0; k < 7; k = k + 1)
      y[k] <= d_b - k * MOD5;
  end
endmodule
