// Problem 31's solver on a board: a 25 MHz clock through the PLL to
// 40 MHz, the answer and cycle count reported on `tx` by `euler_board`.
module top (input clk, output tx, output [7:0] led);
  wire sys, locked, rst, done;
  wire [16:0] answer;
  synth_pll #(.IN_HZ(25000000), .OUT_HZ(40000000)) pll (.clk_in(clk), .clk_out(sys),
    .locked(locked));
  euler31 solver (.clk(sys), .rst(rst), .start(1'b1), .done(done), .answer(answer));
  euler_board #(.CLK_HZ(40000000), .ANSWER_WIDTH(17)) board (.clk(sys), .locked(locked),
    .rst(rst), .done(done), .answer(answer), .tx(tx), .led(led));
endmodule
