// Board shell for the Alchitry Cu around a Project Euler solver (SOLVER, with
// an ANSWER_WIDTH-bit answer, both given with -D).
// Ported from HDLean (https://github.com/nathsou/hdlean, boards/alchitry-cu/euler_top.v).
//
// The solver and its cycle counter run on the 100 MHz oscillator (HDLean's
// shell uses the iCE40 PLL for the fastest clock that meets timing). The UART
// reporter runs on the oscillator divided by two (50 MHz), so it never limits
// the solver's clock; it reads `done` through a two-flop synchronizer, and the
// answer and the cycle count, which no longer change once `done` is set,
// directly. Reset is held while the reset button is pressed and for 16 cycles
// after power-up, then released synchronously in each domain. Once the solver
// is done, "<answer> <cycles>\r\n" is sent at 115200 baud about once a second
// and the LEDs show the answer's low byte.
module euler_top (
  input wire clk,
  input wire rst_n,
  output wire usb_tx,
  output wire [7:0] led
);
  wire fclk = clk;

  // solver domain
  reg [4:0] hold = 5'd0;
  reg [1:0] button = 2'b00;
  always @(posedge fclk) begin
    button <= {button[0], rst_n};
    if (!button[1]) hold <= 5'd0;
    else if (!hold[4]) hold <= hold + 5'd1;
  end
  wire rst = !hold[4];

  wire done;
  wire [`ANSWER_WIDTH-1:0] answer;
  wire [63:0] cycles;
  `SOLVER solver (.clk(fclk), .rst(rst), .start(1'b1), .done(done), .answer(answer));
  euler_cyclecount count (.clk(fclk), .rst(rst), .done(done), .cycles(cycles));

  // reporter domain
  reg slow = 1'b0;
  always @(posedge clk) slow <= !slow;
  reg [1:0] slow_rst = 2'b11;
  reg [1:0] slow_done = 2'b00;
  always @(posedge slow) begin
    slow_rst <= {slow_rst[0], rst};
    slow_done <= {slow_done[0], done};
  end
  euler_report #(.DIVISOR(434)) report (.clk(slow), .rst(slow_rst[1]), .done(slow_done[1]),
    .answer({{(64-`ANSWER_WIDTH){1'b0}}, answer}), .cycles(cycles), .tx(usb_tx), .led(led));
endmodule
