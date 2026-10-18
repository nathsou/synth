// Project Euler 92: how many starting numbers below ten million arrive at 89?
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P092.lean).
//
// A number's chain depends only on the sum s of the squares of its digits,
// s <= 7 * 81 = 567. So instead of ten million chains:
// * seven rounds of the convolution c'[s] = sum_d c[s - d^2] count the 7-digit
//   strings by digit-square sum, streamed two values of s per cycle through a
//   delay line with ten taps for each and two pipelined adder trees,
//   ping-ponging between block RAM banks split by parity;
// * meanwhile a chain engine classifies every s < 200 (does it reach 89?) into
//   a one-bit table indexed by s in BCD, memoised;
// * the last round adds up the c[s] whose s reaches 89: below 200 straight
//   from the table, above it through f(s) < 200 (the digit-square sum of s).
module euler92 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [23:0] answer
);
  localparam W = 24, ROUNDS = 7, LINE = 83, AWB = 9;

  // increment a 3-digit BCD number
  function [11:0] bcd_inc(input [11:0] x);
    reg [3:0] o, t, h;
    begin
      o = x[3:0]; t = x[7:4]; h = x[11:8];
      bcd_inc[3:0] = o == 9 ? 4'd0 : o + 4'd1;
      bcd_inc[7:4] = o == 9 ? (t == 9 ? 4'd0 : t + 4'd1) : t;
      bcd_inc[11:8] = (o == 9 && t == 9) ? h + 4'd1 : h;
    end
  endfunction

  // add two 3-digit BCD numbers (no overflow past 999 here): every digit's sum
  // for both carries in, then the carries select
  function [11:0] bcd_add3(input [11:0] a, input [11:0] b);
    reg carry;
    reg [4:0] s0, s1, x0, x1;
    integer i;
    begin
      carry = 0;
      for (i = 0; i < 3; i = i + 1) begin
        s0 = {1'b0, a[4*i +: 4]} + {1'b0, b[4*i +: 4]};
        s1 = s0 + 5'd1;
        x0 = s0 >= 10 ? s0 - 5'd10 : s0;
        x1 = s0 >= 9 ? s0 - 5'd9 : s1;
        bcd_add3[4*i +: 4] = carry ? x1[3:0] : x0[3:0];
        carry = carry ? s0 >= 9 : s0 >= 10;
      end
    end
  endfunction

  // the square of a decimal digit, in BCD
  function [7:0] sq(input [3:0] d);
    case (d)
      1: sq = 8'h01; 2: sq = 8'h04; 3: sq = 8'h09; 4: sq = 8'h16; 5: sq = 8'h25;
      6: sq = 8'h36; 7: sq = 8'h49; 8: sq = 8'h64; 9: sq = 8'h81; default: sq = 8'h00;
    endcase
  endfunction

  // the digit-square sum of a 3-digit BCD number, in BCD
  function [11:0] dsq(input [11:0] x);
    dsq = bcd_add3(bcd_add3({4'd0, sq(x[3:0])}, {4'd0, sq(x[7:4])}), {4'd0, sq(x[11:8])});
  endfunction

  function [AWB-1:0] mend_of(input [2:0] r);
    mend_of = (81 * (r + 1) + 3) / 2;
  endfunction

  // banks: even and odd s, for rounds of each parity; three copies of the
  // class table (classifier, hi lookup, lo lookup), indexed by BCD
  reg [W-1:0] be0 [0:(1<<AWB)-1];
  reg [W-1:0] bo0 [0:(1<<AWB)-1];
  reg [W-1:0] be1 [0:(1<<AWB)-1];
  reg [W-1:0] bo1 [0:(1<<AWB)-1];
  reg class_a [0:4095];
  reg class_h [0:4095];
  reg class_l [0:4095];

  // control
  reg [2:0] rnd;
  reg [AWB-1:0] m;
  reg [1:0] mode;
  reg [2:0] drain;
  reg [AWB-1:0] mend;
  reg [9:0] lim;
  reg [11:0] sb;
  reg [11:0] ah = 0, al = 0;
  reg ch = 0, cl = 0;
  reg [W-1:0] ps = 0;
  reg pv;
  reg [W-1:0] acc;
  // the classifier (values in BCD)
  reg [11:0] cs, cv;
  reg [11:0] part = 0;
  reg [7:0] sqh = 0;
  reg phase, cdone;
  // pipeline levels 0 ... 4: valid and pair index
  reg [4:0] pv_lvl;
  reg [AWB-1:0] pm [0:4];
  // adder trees, hi (s = 2m + 1) and lo (s = 2m): five, three, two and one partial sums
  reg [W-1:0] ha [0:4], hb [0:2], hc [0:1], hd;
  reg [W-1:0] la [0:4], lb [0:2], lc [0:1], ld;
  // the delay line: x[2m+1], x[2m], x[2m-1], ...
  reg [W-1:0] line [0:LINE-1];

  // the classifier: one chain step per two cycles
  wire cv1 = cv == 12'h001;
  wire cv89 = cv == 12'h089;
  wire below = cv < cs;
  wire known = class_a[cv];
  wire finish_s = cv1 || cv89 || below;
  wire cls_val = cv1 ? 1'b0 : cv89 ? 1'b1 : known;
  wire ph0 = !cdone && !phase;
  wire ph1 = !cdone && phase;
  wire write_cls = ph0 && finish_s;
  wire [11:0] cs_next = bcd_inc(cs);

  // the DP stream: pair m carries x[2m] and x[2m+1]
  wire is_stream = mode == 2'd0;
  wire is_drain = mode == 2'd1;
  wire is_wait = mode == 2'd2;
  wire streaming = rnd < ROUNDS && is_stream;
  wire parity = rnd[0];
  // round r reads the banks written by round r - 1
  wire [W-1:0] ev = parity ? be0[m] : be1[m];
  wire [W-1:0] od = parity ? bo0[m] : bo1[m];
  wire rnd0 = rnd == 0;
  wire [9:0] s_even = {m, 1'b0};
  wire [9:0] s_odd = s_even + 10'd1;
  wire in_e = !(lim < s_even);
  wire in_o = !(lim < s_odd);
  wire [W-1:0] x_e = rnd0 ? {{(W-1){1'b0}}, m == 0} : in_e ? ev : {W{1'b0}};
  wire [W-1:0] x_o = rnd0 ? {W{1'b0}} : in_o ? od : {W{1'b0}};
  wire at_end = m == mend;

  // outputs at level 4: write the next round's banks, or accumulate
  wire out_v = pv_lvl[4];
  wire [AWB-1:0] out_m = pm[4];
  wire last_round = rnd == ROUNDS - 1;
  wire write_en = out_v && !last_round;
  wire w0 = write_en && !parity;
  wire w1 = write_en && parity;
  wire round_end = is_drain && drain == 6;
  wire [2:0] next_rnd = rnd + 3'd1;
  wire [11:0] s_odd_b = sb | 12'd1;
  wire finished = rnd == ROUNDS && !pv && !done;

  integer i;
  always @(posedge clk) begin
    // memories and registers without a reset value
    if (write_cls) begin
      class_a[cs] <= cls_val;
      class_h[cs] <= cls_val;
      class_l[cs] <= cls_val;
    end
    if (w0) begin
      be0[out_m] <= ld;
      bo0[out_m] <= hd;
    end
    if (w1) begin
      be1[out_m] <= ld;
      bo1[out_m] <= hd;
    end
    part <= bcd_add3({4'd0, sq(cv[3:0])}, {4'd0, sq(cv[7:4])});
    sqh <= sq(cv[11:8]);
    ah <= s_odd_b < 12'h200 ? s_odd_b : dsq(s_odd_b);
    al <= sb < 12'h200 ? sb : dsq(sb);
    ch <= class_h[ah];
    cl <= class_l[al];
    ps <= (ch ? hd : {W{1'b0}}) + ((cl && out_m != 0) ? ld : {W{1'b0}});
    pm[0] <= m;
    for (i = 1; i < 5; i = i + 1) pm[i] <= pm[i-1];
    for (i = 0; i < 5; i = i + 1) begin
      ha[i] <= line[2*i*2*i] + line[(2*i+1)*(2*i+1)];
      la[i] <= line[2*i*2*i + 1] + line[(2*i+1)*(2*i+1) + 1];
    end
    hb[0] <= ha[0] + ha[1]; hb[1] <= ha[2] + ha[3]; hb[2] <= ha[4];
    lb[0] <= la[0] + la[1]; lb[1] <= la[2] + la[3]; lb[2] <= la[4];
    hc[0] <= hb[0] + hb[1]; hc[1] <= hb[2];
    lc[0] <= lb[0] + lb[1]; lc[1] <= lb[2];
    hd <= hc[0] + hc[1];
    ld <= lc[0] + lc[1];
  end

  always @(posedge clk)
    if (rst) begin
      rnd <= 0;
      m <= 0;
      mode <= 0;
      drain <= 0;
      mend <= (81 + 3) / 2;
      lim <= 0;
      sb <= 0;
      pv <= 0;
      acc <= 0;
      done <= 0;
      answer <= 0;
      cs <= 12'h001;
      cv <= 12'h001;
      phase <= 0;
      cdone <= 0;
      pv_lvl <= 0;
      for (i = 0; i < LINE; i = i + 1) line[i] <= 0;
    end else begin
      // classifier
      if (write_cls) cs <= cs_next;
      if (write_cls) cv <= cs_next;
      else if (ph1) cv <= bcd_add3(part, {4'd0, sqh});
      phase <= ph0 && !finish_s;
      cdone <= cdone || (write_cls && cs == 12'h199);
      // stream
      if (streaming) m <= at_end ? {AWB{1'b0}} : m + 1'b1;
      line[0] <= streaming ? x_o : {W{1'b0}};
      line[1] <= streaming ? x_e : {W{1'b0}};
      for (i = 2; i < LINE; i = i + 1) line[i] <= line[i-2];
      pv_lvl <= {pv_lvl[3:0], streaming};
      // the last round's classes: sb = BCD of 2m for the pair entering level 3
      if (round_end) sb <= 0;
      else if (pv_lvl[2]) sb <= bcd_inc(bcd_inc(sb));
      pv <= out_v && last_round;
      if (pv) acc <= acc + ps;
      // modes
      drain <= is_drain ? drain + 3'd1 : 3'd0;
      if (is_wait && cdone) mode <= 0;
      else if (round_end) mode <= next_rnd == ROUNDS - 1 ? 2'd2 : 2'd0;
      else if (streaming && at_end) mode <= 1;
      if (round_end) begin
        rnd <= next_rnd;
        lim <= lim + 10'd81;
        mend <= mend_of(next_rnd);
      end
      if (finished) answer <= acc;
      done <= done || finished;
    end
endmodule
