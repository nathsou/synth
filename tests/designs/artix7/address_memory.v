// SPDX-License-Identifier: CERN-OHL-W-2.0
module address_memory(input clk,we, input [7:0] ra,wa,d, output [7:0] q);
  reg [7:0] mem[0:255];
  reg [7:0] addr=3;
  integer i;
  initial for(i=0;i<256;i=i+1) mem[i]=(i*193)^8'h5a;
  always @(posedge clk) begin
    addr<=ra;
    if(we) mem[wa]<=d;
  end
  assign q=mem[addr];
endmodule
