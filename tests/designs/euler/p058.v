// Project Euler 58: the side length of the square spiral at which the share of
// primes along both diagonals first falls below 10%.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P058.lean).
//
// The corners of the layer with odd side s are s^2 - j(s - 1), j = 1, 2, 3.
// The design walks the layers with additions only and tests the corners:
// * a filter: s mod p for the 17 primes p <= 61, stepped by 2 per layer; a
//   small table per prime says which corners p divides;
// * Miller-Rabin lanes for the rest, with bases 2, 7 and 61, in Montgomery
//   form with R = 2^30: squarings as fifteen radix-4 Montgomery steps on a
//   carry-save pair, the multiplications by the base as doublings and additions
//   on one adder;
// * a reorder buffer: jobs go to any idle lane and finish out of order; their
//   verdicts land in a 32-entry buffer, and a collector consumes them in order,
//   layer by layer (a FIFO in block RAM holds each layer's job count).
module euler58 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [15:0] answer
);
  localparam W = 16, NW = 30, LANES = 5, RB = 5, FB = 6, NP = 17;

  function integer filter_prime(input integer i);
    case (i)
      0: filter_prime = 3; 1: filter_prime = 5; 2: filter_prime = 7; 3: filter_prime = 11;
      4: filter_prime = 13; 5: filter_prime = 17; 6: filter_prime = 19; 7: filter_prime = 23;
      8: filter_prime = 29; 9: filter_prime = 31; 10: filter_prime = 37; 11: filter_prime = 41;
      12: filter_prime = 43; 13: filter_prime = 47; 14: filter_prime = 53; 15: filter_prime = 59;
      default: filter_prime = 61;
    endcase
  endfunction

  // which corners s^2 - j(s - 1) (bit j - 1) prime p divides, by s mod p
  function [2:0] divides(input integer p, input [6:0] rsd);
    integer j, v;
    begin
      divides = 0;
      for (j = 0; j < 3; j = j + 1) begin
        v = (rsd * rsd + (j + 1) * (p - (rsd + p - 1) % p)) % p;
        if (v == 0) divides[j] = 1'b1;
      end
    end
  endfunction

  // dispatcher
  reg [W-1:0] s, sm1;
  reg [NW-1:0] sq, cn;
  reg [19:0] f4, t3;
  reg [1:0] cj, lk;
  reg [RB:0] q;
  reg [FB:0] wp, wpd;
  reg filt, hv, pv;
  reg [2:0] hits_r;
  reg [NW-1:0] pn = 0;
  reg [RB-1:0] ptag = 0;
  reg plap = 0;
  reg [6:0] resid [0:NP-1];
  // reorder buffer
  reg [(1<<RB)-1:0] rob_p, rob_l;
  // collector
  reg [RB:0] h;
  reg [FB:0] rptr;
  reg [W-1:0] sc;
  reg [19:0] p10, tt;
  reg [1:0] krem;
  reg kv;
  reg [1:0] layers [0:(1<<FB)-1];

  // the filter: corners divisible by a small prime (from s = 11 on)
  reg [2:0] mask;
  integer i;
  always @* begin
    mask = 0;
    for (i = 0; i < NP; i = i + 1)
      mask = mask | divides(filter_prime(i), resid[i]);
  end
  wire [2:0] hits = filt ? mask : 3'd0;
  wire survives = cj == 3 ? 1'b1 : !hits_r[cj];
  // window and FIFO room
  wire [RB:0] in_flight = q - h;
  wire window_ok = in_flight < (1 << RB);
  wire last_corner = cj == 2;
  wire [FB:0] fifo_used = wp - rptr;
  wire fifo_ok = fifo_used < (1 << FB);

  // lanes: the lowest idle lane takes the slot's job; the lowest finished lane
  // writes its verdict to the reorder buffer
  wire [LANES-1:0] idle, fin;
  wire [RB-1:0] lane_tag [0:LANES-1];
  wire [LANES-1:0] lane_lap, lane_res;
  wire idle_any = |idle;
  wire consumed = pv && idle_any;
  wire slot_free = !pv || consumed;
  wire room_ok = !last_corner || fifo_ok;
  wire walking = hv && !done;
  wire dispatch = walking && survives && slot_free && window_ok && room_ok;
  wire wtaken = |fin;
  genvar l;
  generate
    for (l = 0; l < LANES; l = l + 1) begin : lane
      wire lower_idle, lower_fin;
      if (l == 0) begin : first
        assign lower_idle = 1'b0;
        assign lower_fin = 1'b0;
      end else begin : rest
        assign lower_idle = |idle[l-1:0];
        assign lower_fin = |fin[l-1:0];
      end
      euler58_lane u (.clk(clk), .rst(rst), .load(pv && idle[l] && !lower_idle), .n_in(pn),
        .tag_in(ptag), .lap_in(plap), .granted(fin[l] && !lower_fin), .idle(idle[l]),
        .fin(fin[l]), .tag(lane_tag[l]), .lap(lane_lap[l]), .res(lane_res[l]));
    end
  endgenerate
  reg [RB-1:0] w_tag;
  reg w_lap, w_res;
  reg found_fin;
  always @* begin
    w_tag = 0;
    w_lap = 0;
    w_res = 0;
    found_fin = 0;
    for (i = 0; i < LANES; i = i + 1)
      if (fin[i] && !found_fin) begin
        w_tag = lane_tag[i];
        w_lap = lane_lap[i];
        w_res = lane_res[i];
        found_fin = 1;
      end
  end

  // dispatcher: advance a corner when it is filtered out or dispatched
  wire advance = walking && (!survives || dispatch) && room_ok;
  wire layer_end = advance && last_corner;
  wire [1:0] lk1 = dispatch ? lk + 2'd1 : lk;

  // the collector
  wire [RB-1:0] h_idx = h[RB-1:0];
  wire entry_p = rob_p[h_idx];
  wire ready = rob_l[h_idx] == h[RB];
  wire fifo_has = rptr != wpd;
  wire [1:0] layer_count = layers[rptr[FB-1:0]];
  wire running = !done;
  wire load_k = running && !kv && fifo_has;
  wire k_zero = krem == 0;
  wire consume = running && kv && !k_zero && ready;
  wire layer_done = running && kv && k_zero;
  wire below = p10 < tt;
  wire found = layer_done && below;
  wire next_layer = layer_done && !below;

  integer e;
  always @(posedge clk) begin
    if (layer_end) layers[wp[FB-1:0]] <= lk1;
    if (dispatch) begin
      pn <= cn;
      ptag <= q[RB-1:0];
      plap <= q[RB];
    end
    if (rst) begin
      s <= 3;
      sq <= 9;
      f4 <= 16;
      t3 <= 12;
      sm1 <= 2;
      cn <= 7;
      cj <= 0;
      lk <= 0;
      q <= 0;
      wp <= 0;
      wpd <= 0;
      filt <= 0;
      hits_r <= 0;
      hv <= 0;
      pv <= 0;
      for (i = 0; i < NP; i = i + 1) resid[i] <= 3 % filter_prime(i);
      rob_p <= 0;
      rob_l <= {(1<<RB){1'b1}};
      h <= 0;
      rptr <= 0;
      sc <= 3;
      p10 <= 0;
      tt <= 5;
      krem <= 0;
      kv <= 0;
      done <= 0;
      answer <= 0;
    end else begin
      if (dispatch) q <= q + 1'b1;
      if (layer_end) cn <= sq + t3;
      else if (advance) cn <= cn - sm1;
      if (layer_end) cj <= 0;
      else if (advance) cj <= cj + 2'd1;
      lk <= layer_end ? 2'd0 : lk1;
      if (layer_end) begin
        s <= s + 16'd2;
        sq <= sq + f4;
        f4 <= f4 + 20'd8;
        t3 <= t3 + 20'd6;
        sm1 <= sm1 + 16'd2;
        wp <= wp + 1'b1;
        for (i = 0; i < NP; i = i + 1)
          resid[i] <= resid[i] + 7'd2 >= filter_prime(i) ? resid[i] + 7'd2 - filter_prime(i) : resid[i] + 7'd2;
      end
      wpd <= wp;
      filt <= filt || (layer_end && s == 9);
      if (!hv) hits_r <= hits;
      hv <= !layer_end;
      pv <= dispatch || (pv && !consumed);
      // the reorder buffer
      for (e = 0; e < (1 << RB); e = e + 1)
        if (wtaken && w_tag == e) begin
          rob_p[e] <= w_res;
          rob_l[e] <= w_lap;
        end
      // the collector
      if (consume) h <= h + 1'b1;
      if (load_k) rptr <= rptr + 1'b1;
      if (load_k) krem <= layer_count;
      else if (consume) krem <= krem - 2'd1;
      if (load_k) kv <= 1;
      else if (next_layer) kv <= 0;
      if (consume && entry_p) p10 <= p10 + 20'd10;
      if (next_layer) begin
        sc <= sc + 16'd2;
        tt <= tt + 20'd4;
      end
      if (found) answer <= sc;
      done <= done || found;
    end
  end
endmodule

// A Miller-Rabin lane (bases 2, 7, 61) in Montgomery form, R = 2^30.
module euler58_lane (
  input wire clk,
  input wire rst,
  input wire load,
  input wire [29:0] n_in,
  input wire [4:0] tag_in,
  input wire lap_in,
  input wire granted,
  output wire idle,
  output wire fin,
  output reg [4:0] tag = 0,
  output reg lap = 0,
  output reg res = 0
);
  localparam NW = 30, TW = 32, SW = 35, NS = 18;
  localparam S_IDLE = 0, S_SCAN_R = 1, S_SCAN_T = 2, S_N3 = 3, S_R1 = 4, S_BASE = 5,
    S_DBL = 6, S_ADD = 7, S_ADD_FIX = 8, S_EXP = 9, S_X3 = 10, S_ITER = 11, S_RS = 12,
    S_AD = 13, S_FIN = 14, S_CHK = 15, S_DEC = 16, S_DONE = 17;

  // bit i of a 30-bit value, as HDLean's multiplexer tree picks it (indices 30
  // and 31 read bits 28 and 29)
  function bit_at(input [29:0] x, input [4:0] i);
    bit_at = i >= 30 ? x[28 + i[0]] : x[i];
  endfunction
  function [7:0] base_of(input [1:0] bi);
    case (bi)
      0: base_of = 2; 1: base_of = 7; 2: base_of = 61; default: base_of = 0;
    endcase
  endfunction
  // the bits of the base after its leading one, from the top: their index
  function [2:0] base_start(input [1:0] bi);
    case (bi)
      1: base_start = 1; 2: base_start = 4; default: base_start = 0;
    endcase
  endfunction

  reg [NS-1:0] st;
  reg [NW-1:0] n = 0, r1 = 0, x = 0;
  reg [TW-1:0] x3 = 0, n3 = 0, t = 0;
  reg [SW-1:0] sS = 0, sC = 0;
  reg ci = 0;
  reg [4:0] ib = 0, tb = 0, r = 0, rr = 0, mc = 0;
  reg [2:0] ab = 0;
  reg [1:0] bi = 0, ph = 0, dq = 0;
  reg eq1 = 0, eqm = 0, skip = 0;

  wire is_idle = st[S_IDLE], is_scan_r = st[S_SCAN_R], is_scan_t = st[S_SCAN_T];
  wire is_n3 = st[S_N3], is_r1 = st[S_R1], is_base = st[S_BASE], is_dbl = st[S_DBL];
  wire is_add = st[S_ADD], is_add_fix = st[S_ADD_FIX], is_exp = st[S_EXP], is_x3 = st[S_X3];
  wire is_iter = st[S_ITER], is_rs = st[S_RS], is_ad = st[S_AD], is_fin = st[S_FIN];
  wire is_chk = st[S_CHK], is_dec = st[S_DEC], is_done = st[S_DONE];
  assign idle = is_idle;
  assign fin = is_done;

  wire [TW-1:0] n_w = {2'b00, n};
  wire [TW-1:0] x_w = {2'b00, x};
  // conditions
  wire n_bit = bit_at(n, ib);
  wire mc29 = mc == 29;
  wire mc14 = mc == 14;
  wire a_bit = base_of(bi) >> ab;
  wire ab_zero = ab == 0;
  wire last_base = bi == 2;
  wire exp_end = ib < r;
  wire ph0 = ph == 0, ph1 = ph == 1, ph2 = ph == 2;
  wire found_r = is_scan_r && n_bit;
  wire found_t = is_scan_t && n_bit;
  wire base_go = is_base && !skip;
  wire base_skip = is_base && skip;
  wire r1_done = is_r1 && mc29;
  wire dbl_to_add = is_dbl && a_bit;
  wire dbl_no_add = is_dbl && !a_bit;
  wire dbl_done = dbl_no_add && ab_zero;
  wire dbl_stay = dbl_no_add && !ab_zero;
  wire add_fix_done = is_add_fix && ab_zero;
  wire add_fix_more = is_add_fix && !ab_zero;
  wire mula_done = dbl_done || add_fix_done;
  wire fin_bit = is_fin && ph1;
  wire first = ph1;
  wire pass_d = first ? (eq1 || eqm) : eqm;
  wire last_try = first ? r == 1 : rr == 1;
  wire fail_d = !pass_d && last_try;
  wire cont_d = !pass_d && !last_try;
  wire next_base = (is_dec && pass_d) || base_skip;
  wire prime = next_base && last_base;
  wire composite = is_dec && fail_d;
  wire to_done = prime || composite;

  // the adder, for everything but the Montgomery steps: A + B, gated sources
  wire doubling = is_r1 || is_dbl;
  wire reducing = doubling || is_add_fix || is_fin;
  wire from_x = is_x3 || is_chk;
  wire from_t = is_add || is_add_fix || is_fin;
  wire [TW-1:0] a_op = (doubling ? {t[TW-2:0], 1'b0} : 0) | (from_x ? x_w : 0)
    | (from_t ? t : 0) | (is_ad ? sS[TW-1:0] : 0) | (is_n3 ? n_w : 0);
  // -n = ~n | 1 for odd n
  wire [TW-1:0] neg_n = ~n_w | 1;
  wire [TW-1:0] b_op = (is_add ? x_w : 0) | (is_x3 ? {x_w[TW-2:0], 1'b0} : 0)
    | (is_n3 ? {n_w[TW-2:0], 1'b0} : 0) | (is_ad ? sC[TW-1:0] : 0)
    | (reducing ? neg_n : 0) | (is_chk ? {2'b00, r1} : 0);
  wire [TW-1:0] sum = a_op + b_op;
  wire [TW-1:0] cs = reducing && sum[TW-1] ? a_op : sum;
  wire [NW-1:0] cs_n = cs[NW-1:0];

  // a radix-4 Montgomery step on the carry-save pair (S, C) with pending
  // carry ci: D = d x, q chosen so that S + C + D + q n + ci = 0 mod 4, then
  // shift by two
  wire [SW-1:0] x_s = {5'd0, x};
  wire [SW-1:0] n_s = {5'd0, n};
  wire [SW-1:0] d_x = dq[1] ? (dq[0] ? {3'd0, x3} : {x_s[SW-2:0], 1'b0}) : (dq[0] ? x_s : 0);
  wire [1:0] low = (sS[1:0] + sC[1:0]) + (d_x[1:0] + {1'b0, ci});
  wire [1:0] qv = 2'd0 - low * n[1:0];   // n^-1 = n (mod 4)
  wire [SW-1:0] q_n = qv[1] ? (qv[0] ? {3'd0, n3} : {n_s[SW-2:0], 1'b0}) : (qv[0] ? n_s : 0);
  // in RS, D = Q = 0 and nothing is shifted: S + C + ci becomes exact
  wire [SW-1:0] d_in = is_iter ? d_x : 0;
  wire [SW-1:0] q_in = is_iter ? q_n : 0;
  wire [SW-1:0] s1 = sS ^ sC ^ d_in;
  wire [SW-1:0] c1 = {((sS & sC) | ((sS | sC) & d_in)), 1'b0} | ci;
  wire [SW-1:0] s2 = s1 ^ q_in ^ c1;
  wire [SW-1:0] c2 = {((s1 & q_in) | ((s1 | q_in) & c1)), 1'b0};
  wire cout = c2[1];

  // next state (one-hot)
  wire [NS-1:0] nxt;
  assign nxt[S_IDLE] = (is_idle && !load) || (is_done && granted);
  assign nxt[S_SCAN_R] = (is_idle && load) || (is_scan_r && !n_bit);
  assign nxt[S_SCAN_T] = found_r || (is_scan_t && !n_bit);
  assign nxt[S_N3] = found_t;
  assign nxt[S_R1] = is_n3 || (is_r1 && !mc29);
  assign nxt[S_BASE] = r1_done || (next_base && !last_base);
  assign nxt[S_DBL] = base_go || dbl_stay || add_fix_more || (fin_bit && n_bit);
  assign nxt[S_ADD] = dbl_to_add;
  assign nxt[S_ADD_FIX] = is_add;
  assign nxt[S_EXP] = mula_done || (fin_bit && !n_bit);
  assign nxt[S_X3] = (is_exp && !exp_end) || (is_dec && cont_d);
  assign nxt[S_ITER] = is_x3 || (is_iter && !mc14);
  assign nxt[S_RS] = is_iter && mc14;
  assign nxt[S_AD] = is_rs;
  assign nxt[S_FIN] = is_ad;
  assign nxt[S_CHK] = (is_exp && exp_end) || (is_fin && ph2);
  assign nxt[S_DEC] = is_chk;
  assign nxt[S_DONE] = to_done || (is_done && !granted);

  wire loading = is_idle && load;
  wire t_en = is_r1 || is_dbl || is_add || is_add_fix || is_ad || is_fin;
  wire start_squares = is_dec && first && cont_d;
  // the next Montgomery digit of x, a cycle ahead: x[2i+1 : 2i]
  wire [4:0] mc_next = mc + 5'd1;
  wire [31:0] x_digits = {2'b00, x};

  always @(posedge clk) begin
    if (rst) st <= 1 << S_IDLE;
    else st <= nxt;
    if (loading) begin
      n <= n_in;
      tag <= tag_in;
      lap <= lap_in;
    end
    if (r1_done) r1 <= cs_n;
    if (base_go) x <= r1;
    else if (mula_done || is_fin) x <= cs_n;
    if (is_x3) x3 <= sum;
    if (is_n3) n3 <= sum;
    // t: 1 when the top bit is found (then 30 doublings give R mod n), R mod n
    // for a new base, the doublings and additions, S + C, the final subtraction
    if (found_t) t <= 1;
    else if (base_go) t <= {2'b00, r1};
    else if (t_en) t <= cs;
    if (is_x3) begin
      sS <= 0;
      sC <= 0;
    end else if (is_iter) begin
      sS <= s2 >> 2;
      sC <= c2 >> 2;
    end else if (is_rs) begin
      sS <= s2;
      sC <= c2;
    end
    if (is_x3) ci <= 0;
    else if (is_iter) ci <= cout;
    else if (is_rs) ci <= 0;
    if (loading) ib <= 1;
    else if (is_scan_r && !n_bit) ib <= ib + 5'd1;
    else if (found_r) ib <= 29;
    else if (is_scan_t && !n_bit) ib <= ib - 5'd1;
    else if (mula_done && ph0) ib <= tb - 5'd1;
    else if ((mula_done && ph1) || (fin_bit && !n_bit)) ib <= ib - 5'd1;
    if (found_t) tb <= ib;
    if (found_r) r <= ib;
    if (start_squares) rr <= r - 5'd1;
    else if (is_dec && ph2 && cont_d) rr <= rr - 5'd1;
    if (is_n3 || is_x3) mc <= 0;
    else if (is_r1 || is_iter) mc <= mc + 5'd1;
    if (base_go || (fin_bit && n_bit)) ab <= base_start(bi);
    else if (dbl_stay || add_fix_more) ab <= ab - 3'd1;
    if (r1_done) bi <= 0;
    else if (next_base) bi <= bi + 2'd1;
    if (base_go) ph <= 0;
    else if (mula_done && ph0) ph <= 1;
    else if (start_squares) ph <= 2;
    if (to_done) res <= prime;
    if (is_x3) dq <= x[1:0];
    else if (is_iter) dq <= x_digits[2 * mc_next[3:0] +: 2];
    // n = a: this base says nothing (registered as the base is entered)
    if (r1_done) skip <= n_w == 2;
    else if (next_base) skip <= n_w == {24'd0, base_of(bi + 2'd1)};
    // the checks' comparisons (x is stable): x = R mod n; x + R mod n = n
    eq1 <= x == r1;
    eqm <= sum[NW-1:0] == n && sum[TW-1:NW] == 0;
  end
endmodule
