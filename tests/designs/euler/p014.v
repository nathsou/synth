// Project Euler 14: the start below one million with the longest Collatz chain.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P014.lean).
//
// Fewer starts: only the multiples of 3 in [500000, 750000) and every start in
// [750000, 1000000) can win (any other start lies on a larger start's chain).
// Two steps per cycle: writing x = 4q + r, two steps of the shortcut map give
// (x + B)/4 with B = 0, 2x + 1, 2x + 2 or 8x + 5: one addition. Memoised
// tails: a table of the chain lengths below 2048 is built first in block RAM;
// a lane walks its start only until it drops below 2048, then reads the rest
// from the table. Six lanes take the multiples of 3, eighteen the rest. The
// table is replicated four times, each copy serving six lanes through a
// fixed-priority arbiter and a pipeline that adds the tail and keeps the
// copy's best; at the end the four bests are compared.
module euler14 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [19:0] answer
);
  localparam XW = 36, CW = 10, W = 20, TB = 11, TW = 8, COPIES = 4, L = 24, STRIDE = 18;
  localparam B_WALK = 2'd0, B_WAIT = 2'd1, B_WRITE = 2'd2;

  function integer first_start(input integer l);
    first_start = l < 6 ? 500001 + 3 * l : 750000 + (l - 6);
  endfunction
  function integer last_start(input integer l);
    integer n0, last;
    begin
      n0 = first_start(l);
      last = l < 6 ? 749997 : 999999;
      last_start = n0 + (last - n0) / STRIDE * STRIDE;
    end
  endfunction

  // two shortcut steps: x -> (x + B)/4, and the Collatz steps taken
  function [XW-1:0] step_x(input [XW-1:0] x);
    reg [XW+1:0] xw, b, s;
    begin
      xw = {2'b00, x};
      case (x[1:0])
        2'd0: b = 0;
        2'd3: b = {xw[XW-2:0], 3'b000} | 5;
        default: b = {xw[XW:0], 1'b0} | (x[1:0] == 2'd1 ? 1 : 2);
      endcase
      s = xw + b;
      step_x = s[XW+1:2];
    end
  endfunction
  function [CW-1:0] step_c(input [1:0] r);
    step_c = r == 0 ? 2 : r == 3 ? 4 : 3;
  endfunction

  // the table builder
  reg built;
  reg [TB:0] y;
  reg [XW-1:0] xb;
  reg [CW-1:0] cb;
  reg [1:0] bst;

  // lanes
  reg [XW-1:0] lx [0:L-1];
  reg [CW-1:0] lc [0:L-1];
  reg [W-1:0] ln [0:L-1];
  reg [L-1:0] act;
  // copies: stage 1 (address, count, start), stage 2 (table entry), stage 3
  // (total), and the copy's best
  reg [COPIES-1:0] v1, v2, v3;
  reg [TB-1:0] raddr [0:COPIES-1];
  reg [CW-1:0] c1 [0:COPIES-1], c2 [0:COPIES-1], t3 [0:COPIES-1], bc [0:COPIES-1];
  reg [W-1:0] n1 [0:COPIES-1], n2 [0:COPIES-1], n3 [0:COPIES-1], bn [0:COPIES-1];
  reg [TW-1:0] data [0:COPIES-1];
  reg [TW-1:0] tail0 [0:(1<<TB)-1];
  reg [TW-1:0] tail1 [0:(1<<TB)-1];
  reg [TW-1:0] tail2 [0:(1<<TB)-1];
  reg [TW-1:0] tail3 [0:(1<<TB)-1];

  wire is_walk = bst == B_WALK;
  wire is_wait = bst == B_WAIT;
  wire is_write = bst == B_WRITE;
  wire building = !built;
  wire first = y < 3;
  wire below = xb < {{(XW-TB-1){1'b0}}, y};
  wire issue = building && is_walk && !first && below;
  wire direct = building && is_walk && first;
  wire walk = building && is_walk && !first && !below;
  wire [CW-1:0] cb_sum = cb + {{(CW-TW){1'b0}}, data[0]};
  wire [TW-1:0] bval = direct ? y[TW-1:0] - 1'b1 : cb_sum[TW-1:0];
  wire bwrite = direct || (building && is_write);
  wire [TB:0] y_next = y + 1'b1;
  wire [TB-1:0] y_addr = y[TB-1:0];

  // arbiters: the lowest requesting lane of each copy is granted
  reg [L-1:0] req, granted;
  reg [COPIES-1:0] taken;
  reg [TB-1:0] addr [0:COPIES-1];
  reg [CW-1:0] c_sel [0:COPIES-1];
  reg [W-1:0] n_sel [0:COPIES-1];
  integer l, k;
  always @* begin
    for (l = 0; l < L; l = l + 1)
      req[l] = built && act[l] && lx[l][XW-1:TB] == 0;
    for (k = 0; k < COPIES; k = k + 1) begin
      taken[k] = 0;
      addr[k] = raddr[k];
      c_sel[k] = c1[k];
      n_sel[k] = n1[k];
    end
    for (l = 0; l < L; l = l + 1) begin
      granted[l] = req[l] && !taken[l % COPIES];
      if (granted[l]) begin
        addr[l % COPIES] = lx[l][TB-1:0];
        c_sel[l % COPIES] = lc[l];
        n_sel[l % COPIES] = ln[l];
      end
      taken[l % COPIES] = taken[l % COPIES] || req[l];
    end
    if (issue) addr[0] = xb[TB-1:0];
  end

  // the end: every lane finished and every pipeline drained; the best of the copies
  reg [CW-1:0] best;
  reg [W-1:0] arg;
  always @* begin
    best = bc[0];
    arg = bn[0];
    for (k = 1; k < COPIES; k = k + 1)
      if (best < bc[k]) begin
        best = bc[k];
        arg = bn[k];
      end
  end
  wire finished = built && !(|act || |v1 || |v2 || |v3) && !done;

  always @(posedge clk) begin
    if (bwrite) begin
      tail0[y_addr] <= bval;
      tail1[y_addr] <= bval;
      tail2[y_addr] <= bval;
      tail3[y_addr] <= bval;
    end
    data[0] <= tail0[raddr[0]];
    data[1] <= tail1[raddr[1]];
    data[2] <= tail2[raddr[2]];
    data[3] <= tail3[raddr[3]];
    for (k = 0; k < COPIES; k = k + 1) begin
      raddr[k] <= addr[k];
      c1[k] <= c_sel[k];
      n1[k] <= n_sel[k];
      c2[k] <= c1[k];
      n2[k] <= n1[k];
      t3[k] <= c2[k] + {{(CW-TW){1'b0}}, data[k]};
      n3[k] <= n2[k];
    end
  end

  always @(posedge clk)
    if (rst) begin
      built <= 0;
      y <= 1;
      xb <= 0;
      cb <= 0;
      bst <= B_WALK;
      done <= 0;
      answer <= 0;
      for (l = 0; l < L; l = l + 1) begin
        lx[l] <= first_start(l);
        lc[l] <= 0;
        ln[l] <= first_start(l);
      end
      act <= {L{1'b1}};
      v1 <= 0; v2 <= 0; v3 <= 0;
      for (k = 0; k < COPIES; k = k + 1) begin
        bc[k] <= 0;
        bn[k] <= 0;
      end
    end else begin
      // the builder: walk y until it drops below y, then add the table entry there
      if (bwrite) xb <= {{(XW-TB-1){1'b0}}, y_next};
      else if (walk) xb <= step_x(xb);
      if (bwrite) cb <= 0;
      else if (walk) cb <= cb + step_c(xb[1:0]);
      if (bwrite) y <= y_next;
      if (issue) bst <= B_WAIT;
      else if (is_wait) bst <= B_WRITE;
      else if (is_write) bst <= B_WALK;
      built <= built || (bwrite && y == (1 << TB) - 1);
      // lanes: step, or move on to the next start once granted
      for (l = 0; l < L; l = l + 1) begin
        if (granted[l] && ln[l] != last_start(l)) begin
          lx[l] <= ln[l] + STRIDE;
          lc[l] <= 0;
          ln[l] <= ln[l] + STRIDE;
        end else if (act[l] && lx[l][XW-1:TB] != 0) begin
          lx[l] <= step_x(lx[l]);
          lc[l] <= lc[l] + step_c(lx[l][1:0]);
        end
        act[l] <= act[l] && (!granted[l] || ln[l] != last_start(l));
      end
      // copies
      v1 <= taken;
      v2 <= v1;
      v3 <= v2;
      for (k = 0; k < COPIES; k = k + 1)
        if (v3[k] && bc[k] < t3[k]) begin
          bc[k] <= t3[k];
          bn[k] <= n3[k];
        end
      if (finished) answer <= arg;
      done <= done || finished;
    end
endmodule
