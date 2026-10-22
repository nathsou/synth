// SPDX-License-Identifier: CERN-OHL-W-2.0
`timescale 1ns/1ps
module tb;
  reg clk = 0, en = 0, rst = 0, arst = 1;
  reg [7:0] d = 0;
  wire [7:0] refq, got;
  controls refdut(clk, en, rst, arst, d, refq);
  controls_net dut(clk, en, rst, arst, d, got);
  integer i;
  initial begin
    #1;
    if (got !== refq) $fatal(1, "initial values: %h != %h", got, refq);
    for (i = 0; i < 1024; i = i + 1) begin
      en = (i >> 1) & 1; rst = (i >> 2) & 1; arst = (i >> 3) & 1;
      d = (i * 73) ^ (i >> 2);
      #1;
      if (got !== refq) $fatal(1, "async controls at %d: %h != %h", i, got, refq);
      clk = !clk;
      #1;
      if (got !== refq) $fatal(1, "clock at %d: %h != %h", i, got, refq);
    end
    $display("PASS controls");
    $finish;
  end
endmodule
