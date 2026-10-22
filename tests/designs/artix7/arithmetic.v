// SPDX-License-Identifier: CERN-OHL-W-2.0
module arithmetic #(parameter W=5)(input [W-1:0] a, b, input cin,
  output [W:0] sum, output [W-1:0] diff, output lt, slt, output [W-1:0] muxed);
  assign sum = {1'b0,a} + {1'b0,b} + cin;
  assign diff = a - b;
  assign lt = a < b;
  assign slt = $signed(a) < $signed(b);
  assign muxed = cin ? (a ^ b) : (a & b);
endmodule
