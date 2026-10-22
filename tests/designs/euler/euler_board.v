// A board-neutral shell for a Project Euler solver, for the playground: the
// solver runs on `clk` (CLK_HZ, from the board's PLL) once `locked` is high,
// and once it is done the UART reporter sends "<answer> <cycles>\r\n" at
// 115200 baud on `tx` again and again, and the LEDs show the answer's low
// byte. Reset is held while the PLL locks and for 16 cycles after, as
// boards' buttons differ in polarity.
//
// The playground's top for a solver instantiates it with this shell:
//
//   module top #(parameter CLK_HZ = 100000000, parameter MAX_HZ = 40000000)
//     (input clk, output tx, output [7:0] led);
//     localparam SYS_HZ = MAX_HZ < CLK_HZ ? MAX_HZ : CLK_HZ;
//     wire sys, locked, rst, done;
//     wire [16:0] answer;
//     synth_pll #(.IN_HZ(CLK_HZ), .OUT_HZ(SYS_HZ)) pll (.clk_in(clk), .clk_out(sys),
//       .locked(locked));
//     euler31 solver (.clk(sys), .rst(rst), .start(1'b1), .done(done), .answer(answer));
//     euler_board #(.CLK_HZ(SYS_HZ), .ANSWER_WIDTH(17)) board (.clk(sys), .locked(locked),
//       .rst(rst), .done(done), .answer(answer), .tx(tx), .led(led));
//   endmodule
module euler_board #(
  parameter CLK_HZ = 100000000,
  parameter ANSWER_WIDTH = 64
) (
  input wire clk,
  input wire locked,
  output wire rst,
  input wire done,
  input wire [ANSWER_WIDTH-1:0] answer,
  output wire tx,
  output wire [7:0] led
);
  // `locked` comes from the PLL, asynchronous to `clk`.
  reg [1:0] lock_sync = 2'b00;
  always @(posedge clk or negedge locked)
    if (!locked) lock_sync <= 2'b00; else lock_sync <= {lock_sync[0], 1'b1};
  reg [4:0] hold = 5'd0;
  always @(posedge clk)
    if (!lock_sync[1]) hold <= 5'd0;
    else if (!hold[4]) hold <= hold + 5'd1;
  assign rst = !hold[4];

  wire [63:0] cycles;
  euler_cyclecount count (.clk(clk), .rst(rst), .done(done), .cycles(cycles));
  euler_report #(.DIVISOR(CLK_HZ / 115200)) report (.clk(clk), .rst(rst), .done(done),
    .answer({{(64-ANSWER_WIDTH){1'b0}}, answer}), .cycles(cycles), .tx(tx), .led(led));
endmodule
