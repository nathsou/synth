// Cycle counter and UART reporter for the Project Euler solvers.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/Report.lean).

// Counts cycles from reset release until `done`: four 16-bit chunks; the wrap
// of chunk 0 is predicted a cycle ahead and the "all ones" flags of chunks 1
// and 2 are registered, so every path is one LUT and one 16-bit carry chain.
module euler_cyclecount (
  input wire clk,
  input wire rst,
  input wire done,
  output wire [63:0] cycles
);
  reg [15:0] c0, c1, c2, c3;
  reg w0, a1, a2;
  wire run = !done;
  wire inc2 = w0 && a1;
  wire inc3 = inc2 && a2;
  always @(posedge clk)
    if (rst) begin
      c0 <= 0; c1 <= 0; c2 <= 0; c3 <= 0;
      w0 <= 0; a1 <= 0; a2 <= 0;
    end else begin
      if (run) c0 <= c0 + 16'd1;
      w0 <= run && c0 == 16'hFFFE;
      if (w0) c1 <= c1 + 16'd1;
      if (inc2) c2 <= c2 + 16'd1;
      if (inc3) c3 <= c3 + 16'd1;
      a1 <= c1 == 16'hFFFF;
      a2 <= c2 == 16'hFFFF;
    end
  assign cycles = {c3, c2, c1, c0};
endmodule

// Once `done` is seen, converts the answer and then the cycle count to decimal
// (serial double dabble, 64 steps each) and sends "<answer> <cycles>\r\n" over
// the UART (8N1, DIVISOR clocks per bit), again and again with a pause between
// messages. Leading zeros are not sent. The LEDs show the answer's low byte.
module euler_report #(
  parameter DIVISOR = 434
) (
  input wire clk,
  input wire rst,
  input wire done,
  input wire [63:0] answer,
  input wire [63:0] cycles,
  output wire tx,
  output reg [7:0] led
);
  localparam S_WAIT = 3'd0, S_LOAD = 3'd1, S_CONV = 3'd2, S_SEND = 3'd3, S_GAP = 3'd4;
  localparam DIGITS = 20;

  reg [2:0] state;
  reg which;
  reg [63:0] val = 0;
  reg [79:0] bcd = 0;
  reg [6:0] steps = 0;
  reg [4:0] ptr = 0;
  reg started = 0;
  reg [1:0] crlf = 0;
  reg [9:0] sr;
  reg [3:0] bits = 0;
  reg [15:0] baud = 0;
  reg busy;
  reg [25:0] gap = 0;

  // double dabble: add 3 to every digit that is at least 5, then shift
  reg [79:0] adj;
  integer i;
  always @* begin
    for (i = 0; i < 20; i = i + 1)
      adj[4*i +: 4] = bcd[4*i +: 4] >= 4'd5 ? bcd[4*i +: 4] + 4'd3 : bcd[4*i +: 4];
  end

  wire is_wait = state == S_WAIT;
  wire is_load = state == S_LOAD;
  wire is_conv = state == S_CONV;
  wire is_send = state == S_SEND;
  wire is_gap = state == S_GAP;
  wire latch = is_wait && done;
  wire conv_last = steps == 7'd63;

  // sending: 20 digits from the top of the BCD register, then a space (after
  // the answer) or CR LF (after the cycle count)
  wire in_digits = ptr < DIGITS;
  wire [3:0] dig = bcd[79:76];
  wire last_digit = ptr == DIGITS - 1;
  wire vis = started || dig != 0 || last_digit;
  wire is_second = crlf == 2'd1;
  wire [7:0] sep_char = which ? (is_second ? 8'd10 : 8'd13) : 8'd32;
  wire [7:0] ch = in_digits ? {4'd0, dig} + 8'd48 : sep_char;
  wire visible = in_digits ? vis : 1'b1;
  wire take = is_send && !busy;
  wire load = take && visible;
  wire sep_done = which ? is_second : 1'b1;
  wire number_end = take && !in_digits && sep_done;
  wire tick = busy && baud == 0;
  wire take_digit = take && in_digits;
  wire to_gap = number_end && which;
  wire next_number = number_end && !which;

  // registers with a reset value
  always @(posedge clk) begin
    if (rst) begin
      state <= S_WAIT;
      which <= 0;
      sr <= 10'h3FF;
      busy <= 0;
      led <= 0;
    end else begin
      if (load) sr <= {1'b1, ch, 1'b0};
      else if (tick) sr <= {1'b1, sr[9:1]};
      if (load) busy <= 1;
      else if (tick && bits == 4'd1) busy <= 0;
      if (latch) state <= S_LOAD;
      else if (is_load) state <= S_CONV;
      else if (is_conv && conv_last) state <= S_SEND;
      else if (next_number) state <= S_LOAD;
      else if (to_gap) state <= S_GAP;
      else if (is_gap && gap == 0) state <= S_LOAD;
      if (latch) which <= 0;
      else if (next_number) which <= 1;
      else if (to_gap) which <= 0;
      if (latch) led <= answer[7:0];
    end
  end

  // registers without one
  always @(posedge clk) begin
    if (load) bits <= 4'd10;
    else if (tick) bits <= bits - 4'd1;
    if (load || tick) baud <= DIVISOR - 1;
    else if (busy) baud <= baud - 16'd1;
    if (is_load) ptr <= 0;
    else if (take_digit) ptr <= ptr + 5'd1;
    if (is_load) started <= 0;
    else if (take_digit) started <= started || vis;
    if (is_load) crlf <= 0;
    else if (take && !in_digits) crlf <= crlf + 2'd1;
    if (to_gap) gap <= 26'h3FFFFFF;
    else if (is_gap) gap <= gap - 26'd1;
    if (is_load) val <= which ? cycles : answer;
    else if (is_conv) val <= {val[62:0], 1'b0};
    if (is_load) bcd <= 0;
    else if (is_conv) bcd <= {adj[78:0], val[63]};
    else if (take_digit) bcd <= {bcd[75:0], 4'd0};
    if (is_load) steps <= 0;
    else if (is_conv) steps <= steps + 7'd1;
  end
  assign tx = sr[0];
endmodule
