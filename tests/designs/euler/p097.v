// Project Euler 97: the last ten digits of 28433 * 2^7830457 + 1.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P097.lean).
//
// Modulo 10^10 = 2^10 * 5^10, by the Chinese remainder theorem. Modulo 5^10
// (24-bit residues): left-to-right square-and-multiply, where multiplying by
// two is a doubling with one conditional subtraction, so only the 23 squarings
// and the final multiplication by 28433 need the modular multiplier: radix 4,
// interleaved (acc <- 4 acc + d B mod M, two bits of A per cycle, twelve
// cycles), with the seven candidate reductions t - k M computed side by side
// and the largest non-negative one kept. Modulo 2^10 the same steps run on a
// ten-bit register. The residues are combined at the end:
// X = a + 5^10 ((b - a) 5^-10 mod 2^10), serially.
module euler97 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [33:0] answer
);
  localparam RW = 24, YW = 28, W = 34;
  localparam [27:0] M5 = 28'd9765625;
  localparam [22:0] E = 23'd7830457;
  localparam [23:0] K = 24'd28433;
  localparam [9:0] INV5 = 10'd841;   // 5^-10 mod 2^10

  localparam S_SQUARE = 3'd0, S_PREP = 3'd1, S_MUL = 3'd2, S_AFTER = 3'd3,
    S_DOUBLE = 3'd4, S_FINAL = 3'd5, S_FINISH = 3'd6, S_CRT = 3'd7;

  reg [2:0] state;
  reg [RW-1:0] r;
  reg [RW-1:0] a = 0;
  reg [RW:0] b = 0;
  reg [RW+1:0] b3 = 0;
  reg [RW-1:0] acc = 0;
  reg [4:0] cnt = 0;
  reg [4:0] idx;
  reg last;
  reg [9:0] r2, tt;
  reg [W-1:0] x, mb;
  // the candidate reductions for the next digit
  reg [YW-1:0] y [0:6];

  wire is_square = state == S_SQUARE;
  wire is_prep = state == S_PREP;
  wire is_mul = state == S_MUL;
  wire is_after = state == S_AFTER;
  wire is_double = state == S_DOUBLE;
  wire is_final = state == S_FINAL;
  wire is_finish = state == S_FINISH;
  wire is_crt = state == S_CRT;

  // one radix-4 step: the candidates 4 acc + d B - k M, keep the last non-negative one
  wire [YW-1:0] acc4 = {2'b00, acc, 2'b00};
  reg [RW-1:0] red;
  reg [YW-1:0] t;
  integer k;
  always @* begin
    t = acc4 + y[0];
    red = t[RW-1:0];
    for (k = 1; k < 7; k = k + 1) begin
      t = acc4 + y[k];
      if (!t[YW-1]) red = t[RW-1:0];
    end
  end

  // the reductions d B - k M for the next digit (or the first one, in S_PREP)
  wire [1:0] d_next = is_prep ? a[RW-1:RW-2] : a[RW-3:RW-4];
  wire [YW-1:0] b_y = {{(YW-RW-1){1'b0}}, b};
  wire [YW-1:0] lo = d_next[0] ? b_y : {YW{1'b0}};
  wire [YW-1:0] hi = d_next[0] ? {{(YW-RW-2){1'b0}}, b3} : {b_y[YW-2:0], 1'b0};
  wire [YW-1:0] d_b = d_next[1] ? hi : lo;

  // doubling: r = 2r mod M
  wire [RW:0] r2x = {r, 1'b0};
  wire [RW-1:0] r_dbl = r2x >= M5 ? r2x - M5 : r2x;
  wire e_bit = E[idx];
  wire idx0 = idx == 0;
  wire cnt0 = cnt == 0;
  wire start_mul = is_square || is_final;
  wire after_sq = is_after && !last;
  wire leave_bit = (after_sq && !e_bit) || is_double;

  // the residues of K 2^E + 1: a = acc + 1 mod 5^10, b = K r2 + 1 mod 2^10
  wire [RW-1:0] a_res = acc == M5 - 1 ? {RW{1'b0}} : acc + 1'b1;
  wire [9:0] b_res = r2 * K[9:0] + 10'd1;
  wire [9:0] t_val = (b_res - a_res[9:0]) * INV5;
  wire finished = is_crt && tt == 0 && !done;

  always @(posedge clk) begin
    if (rst) begin
      state <= S_SQUARE;
      r <= 1;
      idx <= 5'd22;
      last <= 0;
      done <= 0;
      answer <= 0;
      r2 <= 1;
      tt <= 0;
      x <= 0;
      mb <= 0;
    end else begin
      if (start_mul) state <= S_PREP;
      else if (is_prep) state <= S_MUL;
      else if (is_mul) state <= cnt0 ? S_AFTER : S_MUL;
      else if (is_after) state <= last ? S_FINISH : e_bit ? S_DOUBLE : idx0 ? S_FINAL : S_SQUARE;
      else if (is_double) state <= idx0 ? S_FINAL : S_SQUARE;
      else if (is_finish) state <= S_CRT;
      if (is_double) r <= r_dbl;
      else if (after_sq) r <= acc;
      if (after_sq) r2 <= r2 * r2;
      else if (is_double) r2 <= {r2[8:0], 1'b0};
      if (leave_bit) idx <= idx - 5'd1;
      last <= last || is_final;
      if (is_finish) tt <= t_val;
      else if (is_crt) tt <= tt >> 1;
      if (is_finish) x <= {{(W-RW){1'b0}}, a_res};
      else if (is_crt && tt[0]) x <= x + mb;
      if (is_finish) mb <= M5;
      else if (is_crt) mb <= {mb[W-2:0], 1'b0};
      if (finished) answer <= x;
      done <= done || finished;
    end
  end

  always @(posedge clk) begin
    if (start_mul) a <= is_final ? K : r;
    else if (is_mul) a <= {a[RW-3:0], 2'b00};
    if (start_mul) b <= {1'b0, r};
    if (start_mul) b3 <= {2'b00, r} + {1'b0, r, 1'b0};
    if (start_mul) acc <= 0;
    else if (is_mul) acc <= red;
    if (start_mul) cnt <= 5'd11;
    else if (is_mul) cnt <= cnt - 5'd1;
    for (k = 0; k < 7; k = k + 1)
      y[k] <= d_b - k * M5;
  end
endmodule
