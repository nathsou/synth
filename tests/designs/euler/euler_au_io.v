// Board shell for the Alchitry Au (V1) with the Io V1 shield around a Project
// Euler solver (SOLVER, with an ANSWER_WIDTH-bit answer, both given with -D).
//
// As on the Cu (euler_top.v), the solver and its cycle counter run on the
// 100 MHz oscillator, reset is held while the reset button is pressed and for
// 16 cycles after power-up, and once the solver is done the UART reporter sends
// "<answer> <cycles>\r\n" at 115200 baud about once a second.
//
// The Io shield shows the result in decimal on its four-digit display, four
// digits at a time with leading zeros blanked: the least significant digits
// first, and any button pressed shows the next four (the Au's LEDs show which
// page, one-hot). The first DIP switch shows the cycle count instead of the
// answer. The shield's 24 LEDs show the shown value's low 24 bits in binary.
// The display shows dashes until the solver is done.
module euler_au_io (
  input wire clk,
  input wire rst_n,
  output wire usb_tx,
  output wire [7:0] led,
  output wire [23:0] io_led,
  output reg [7:0] io_segment,  // active low: decimal point, then g..a
  output reg [3:0] io_select,   // active low, bit 0 the rightmost digit
  input wire [4:0] io_button,   // active high, pulled down
  input wire io_dip             // the first DIP switch, active high, pulled down
);
  // solver domain
  reg [4:0] hold = 5'd0;
  reg [1:0] button = 2'b00;
  always @(posedge clk) begin
    button <= {button[0], rst_n};
    if (!button[1]) hold <= 5'd0;
    else if (!hold[4]) hold <= hold + 5'd1;
  end
  wire rst = !hold[4];

  wire done;
  wire [`ANSWER_WIDTH-1:0] answer;
  wire [63:0] cycles;
  `SOLVER solver (.clk(clk), .rst(rst), .start(1'b1), .done(done), .answer(answer));
  euler_cyclecount count (.clk(clk), .rst(rst), .done(done), .cycles(cycles));

  // reporter domain (50 MHz), as on the Cu
  reg slow = 1'b0;
  always @(posedge clk) slow <= !slow;
  reg [1:0] slow_rst = 2'b11;
  reg [1:0] slow_done = 2'b00;
  always @(posedge slow) begin
    slow_rst <= {slow_rst[0], rst};
    slow_done <= {slow_done[0], done};
  end
  wire [7:0] low_byte;
  euler_report #(.DIVISOR(434)) report (.clk(slow), .rst(slow_rst[1]), .done(slow_done[1]),
    .answer({{(64-`ANSWER_WIDTH){1'b0}}, answer}), .cycles(cycles), .tx(usb_tx), .led(low_byte));

  // Io shield, in the solver domain. Buttons and the switch are synchronized
  // and sampled every 2^20 cycles (10 ms), which also debounces them.
  reg [1:0] dip = 2'b00;
  reg [9:0] buttons = 10'd0;
  reg [19:0] tick = 20'd0;
  reg pressed = 1'b0, press = 1'b0;
  always @(posedge clk) begin
    dip <= {dip[0], io_dip};
    buttons <= {buttons[4:0], io_button};
    tick <= tick + 20'd1;
    press <= 1'b0;
    if (tick == 20'd0) begin
      pressed <= |buttons[9:5];
      press <= |buttons[9:5] && !pressed;
    end
  end
  wire [63:0] value = dip[1] ? cycles : {{(64-`ANSWER_WIDTH){1'b0}}, answer};

  // Decimal conversion of the shown value (serial double dabble, 64 steps),
  // started again whenever the value changes.
  reg [63:0] shown = 64'd0, bin = 64'd0;
  reg [79:0] bcd = 80'd0, digits = 80'd0;
  reg [6:0] steps = 7'd0;
  reg ready = 1'b0;
  function [79:0] adjust(input [79:0] x);
    integer k;
    begin
      for (k = 0; k < 20; k = k + 1)
        adjust[4*k +: 4] = x[4*k +: 4] >= 4'd5 ? x[4*k +: 4] + 4'd3 : x[4*k +: 4];
    end
  endfunction
  wire [143:0] next = {adjust(bcd), bin} << 1;
  always @(posedge clk)
    if (rst) begin
      steps <= 7'd0;
      ready <= 1'b0;
      shown <= 64'd0;
    end else if (steps == 7'd0) begin
      if (value != shown || !ready) begin
        shown <= value;
        bin <= value;
        bcd <= 80'd0;
        steps <= 7'd64;
        ready <= 1'b0;
      end
    end else begin
      {bcd, bin} <= next;
      steps <= steps - 7'd1;
      if (steps == 7'd1) begin
        digits <= next[143:64];
        ready <= 1'b1;
      end
    end

  // Pages of four digits, up to the most significant non-zero digit.
  reg [4:0] used;
  integer j;
  always @* begin
    used = 5'd1;
    for (j = 1; j < 20; j = j + 1)
      if (digits[4*j +: 4] != 4'd0) used = j + 1;
  end
  wire [2:0] pages = (used + 5'd3) >> 2;
  reg [2:0] page = 3'd0;
  always @(posedge clk)
    if (rst) page <= 3'd0;
    else if (press) page <= page + 3'd1 >= pages ? 3'd0 : page + 3'd1;
    else if (page >= pages) page <= 3'd0;

  // Multiplexed display: each digit lit for 2^16 cycles (0.66 ms).
  reg [17:0] scan = 18'd0;
  always @(posedge clk) scan <= scan + 18'd1;
  wire [1:0] position = scan[17:16];
  wire [4:0] index = {page, position};
  wire [3:0] digit = digits[4*index +: 4];
  reg [6:0] segments;
  always @* begin
    case (digit)
      4'd0: segments = 7'b0111111;
      4'd1: segments = 7'b0000110;
      4'd2: segments = 7'b1011011;
      4'd3: segments = 7'b1001111;
      4'd4: segments = 7'b1100110;
      4'd5: segments = 7'b1101101;
      4'd6: segments = 7'b1111101;
      4'd7: segments = 7'b0000111;
      4'd8: segments = 7'b1111111;
      4'd9: segments = 7'b1100111;
      default: segments = 7'b0000000;
    endcase
    if (!(done && ready)) segments = 7'b1000000;
    else if (index >= used) segments = 7'b0000000;
  end
  always @(posedge clk) begin
    io_segment <= ~{1'b0, segments};
    io_select <= ~(4'b0001 << position);
  end

  assign io_led = done && ready ? shown[23:0] : 24'd0;
  assign led = done ? 8'b1 << page : 8'd0;
endmodule
