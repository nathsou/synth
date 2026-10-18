// Project Euler 57: in the first thousand expansions p/q of the continued
// fraction of sqrt(2) (p' = p + 2q, q' = p + q), how many numerators have more
// digits than their denominators?
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P057.lean).
//
// Exact arithmetic on numbers of up to 384 digits: p and q live in two block
// RAM arrays of 32-digit BCD words (128 bits, one word per address). Each
// expansion streams the words from the bottom, one per cycle, through a
// pipeline: the block RAMs' output registers (R), a second register (S),
// q' = p + q (A, one BCD addition with carry in), p' = q' + q (B, written back
// in place with q'), and the digit counts from the highest nonzero words (C).
// The expansions follow each other without draining the pipeline.
module euler57 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [9:0] answer
);
  localparam D = 32, BW = 4 * D, AW = 4, LW = 10;
  localparam PH_CLEAR = 2'd0, PH_RUN = 2'd1, PH_DRAIN = 2'd2, PH_DONE = 2'd3;

  // the number of digits of a word: the position of its top nonzero digit, plus one
  function [5:0] word_len(input [BW-1:0] x);
    integer i;
    begin
      word_len = 0;
      for (i = 0; i < D; i = i + 1)
        if (x[4*i +: 4] != 0) word_len = i + 1;
    end
  endfunction

  reg [BW-1:0] pm [0:(1<<AW)-1];
  reg [BW-1:0] qm [0:(1<<AW)-1];

  reg [1:0] ph;
  reg [9:0] it;
  reg [4:0] w, wl, top;
  reg vr, vs, va, vc;
  reg [AW-1:0] wr = 0, ws = 0, wa = 0, wc = 0;
  reg [BW-1:0] pr = 0, qr = 0, ps = 0, qs = 0, qa = 0, qqa = 0, pc = 0, qc = 0;
  reg cq = 0, cp = 0;
  reg [LW-1:0] plen, qlen, count;

  wire is_clear = ph == PH_CLEAR;
  wire is_run = ph == PH_RUN;
  wire is_drain = ph == PH_DRAIN;
  wire [AW-1:0] wa_addr = w[AW-1:0];

  // stage A: q' = p + q + carry (none into word 0)
  wire [BW+3:0] s_a;
  euler_bcd_add #(.N(D), .CARRY_IN(1)) add_a (.a(ps), .b(qs), .cin(cq && ws != 0), .s(s_a));
  // stage B: p' = q' + q + carry, written back in place with q'
  wire [BW+3:0] s_b;
  euler_bcd_add #(.N(D), .CARRY_IN(1)) add_b (.a(qa), .b(qqa), .cin(cp && wa != 0), .s(s_b));
  wire [BW-1:0] q_new = s_a[BW-1:0];
  wire [BW-1:0] p_new = s_b[BW-1:0];

  // stage C: the digit counts from the highest nonzero words; word 0 starts a
  // new expansion, so the previous one is counted then
  wire [LW-1:0] base = {{(LW-AW-5){1'b0}}, wc, 5'd0};
  wire p_nz = pc != 0;
  wire q_nz = qc != 0;
  wire first = vc && wc == 0;
  wire longer = qlen < plen;

  wire w0 = w == 0;
  wire clear_last = w == (1 << AW) - 1;
  wire last_word = is_run && w == wl;
  wire last_it = it == 999;
  wire drained = is_drain && !(vr || vs || va || vc);
  wire finished = drained && !done;
  wire write = is_clear || va;
  wire [AW-1:0] waddr = is_clear ? wa_addr : wa;

  always @(posedge clk) begin
    if (write) begin
      pm[waddr] <= is_clear ? (w0 ? 128'd3 : 128'd0) : p_new;
      qm[waddr] <= is_clear ? (w0 ? 128'd2 : 128'd0) : qa;
    end
    // the pipeline: R (the block RAMs' outputs), S, A, B, C
    wr <= wa_addr;
    pr <= pm[wa_addr];
    qr <= qm[wa_addr];
    ws <= wr; ps <= pr; qs <= qr;
    wa <= ws; qa <= q_new; qqa <= qs;
    wc <= wa; pc <= p_new; qc <= qa;
    if (vs) cq <= s_a[BW +: 4] != 0;
    if (va) cp <= s_b[BW +: 4] != 0;
  end

  always @(posedge clk)
    if (rst) begin
      ph <= PH_CLEAR;
      it <= 1;
      w <= 0;
      wl <= 3;
      top <= 0;
      vr <= 0; vs <= 0; va <= 0; vc <= 0;
      plen <= 1;
      qlen <= 1;
      count <= 0;
      done <= 0;
      answer <= 0;
    end else begin
      vr <= is_run; vs <= vr; va <= vs; vc <= va;
      if (vc && p_nz) plen <= base + word_len(pc);
      else if (first) plen <= 0;
      if (vc && q_nz) qlen <= base + word_len(qc);
      else if (first) qlen <= 0;
      if (vc && p_nz && top < {1'b0, wc}) top <= {1'b0, wc};
      // expansion i + 1 reads words 0 ... wl: at least four, one past the top
      if (is_clear) w <= clear_last ? 5'd0 : w + 5'd1;
      else if (is_run) w <= last_word ? 5'd0 : w + 5'd1;
      if (last_word) begin
        wl <= top == 0 ? 5'd3 : top + 5'd2;
        it <= it + 10'd1;
      end
      if (is_clear && clear_last) ph <= PH_RUN;
      else if (last_word && last_it) ph <= PH_DRAIN;
      else if (drained) ph <= PH_DONE;
      if (first && longer) count <= count + 10'd1;
      if (finished) answer <= longer ? count + 10'd1 : count;
      done <= done || finished;
    end
endmodule
