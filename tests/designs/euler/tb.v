// Testbench: runs a solver until `done` and prints the answer and the cycles
// from reset release, like the HDLean flow. EXPECT and MAX_CYCLES are given
// with -D (the answer in decimal).
`timescale 1ns/1ps
module tb;
  reg clk = 0;
  reg rst = 1;
  wire done;
  wire [63:0] answer;
  wire [`ANSWER_WIDTH-1:0] ans;
  `SOLVER dut (.clk(clk), .rst(rst), .start(1'b1), .done(done), .answer(ans));
  assign answer = ans;
  always #5 clk = !clk;
  integer cycles = 0;
  initial begin
    @(posedge clk); #1 rst = 0;
    while (!done && cycles < `MAX_CYCLES) begin
      @(posedge clk); #1 cycles = cycles + 1;
    end
    if (done && answer == `EXPECT) $display("PASS answer %0d after %0d cycles", answer, cycles);
    else $display("FAIL answer %0d done %b after %0d cycles (expected %0d)", answer, done, cycles, `EXPECT);
    $finish;
  end
endmodule
