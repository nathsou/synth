// SPDX-License-Identifier: CERN-OHL-W-2.0
`timescale 1ns/1ps
module tb;
  reg clk=0,we=0;
  reg [7:0] ra=0,wa=0,d=0;
  wire [7:0] refq,got;
  address_memory refdut(clk,we,ra,wa,d,refq);
  address_memory_net dut(clk,we,ra,wa,d,got);
  integer i;
  initial begin
    #1;
    if(got !== refq) $fatal(1,"initial address read: %h != %h",got,refq);
    for(i=0;i<1024;i=i+1) begin
      ra=i*13; wa=(i%3==0) ? ra : i*71; d=i*117; we=(i%5 != 0);
      #5 clk=1;
      #1;
      if(got !== refq) $fatal(1,"transparent read %d: %h != %h",i,got,refq);
      #5 clk=0;
    end
    $display("PASS address memory"); $finish;
  end
endmodule
