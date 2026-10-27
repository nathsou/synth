module top(input clk, input btn, output [7:0] led);
  reg [25:0] ctr;
  always @(posedge clk) ctr <= btn ? 0 : ctr + 1;
  assign led = ctr[25:18];
endmodule
