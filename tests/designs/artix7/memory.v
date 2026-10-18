// SPDX-License-Identifier: CERN-OHL-W-2.0
module memory #(parameter W=18, D=128, OFFSET=0, NEG=0, MASK=0)(
    input clk, re, input [W-1:0] we, input [15:0] ra, wa,
    input [W-1:0] d, output reg [W-1:0] q = {W{1'b1}}, output reg [W-1:0] q2 = 0);
  reg [W-1:0] mem [OFFSET:OFFSET+D-1];
  integer i;
  wire [W-1:0] old = mem[wa];
  wire [W-1:0] masked;
  genvar b;
  generate for (b=0; b<W; b=b+1) begin : mask_bits
    assign masked[b] = we[b] ? d[b] : old[b];
  end endgenerate
  initial for (i=OFFSET; i<OFFSET+D; i=i+1) mem[i] = (i * 193) ^ (i << 9) ^ 64'h96e7b4a52d3c185a;
  generate if (NEG) begin
    always @(negedge clk) begin
      if (re) begin q <= mem[ra]; q2 <= mem[wa]; end
      if (MASK) mem[wa] <= masked;
      else if (we[0]) mem[wa] <= d;
    end
  end else begin
    always @(posedge clk) begin
      if (re) begin q <= mem[ra]; q2 <= mem[wa]; end
      if (MASK) mem[wa] <= masked;
      else if (we[0]) mem[wa] <= d;
    end
  end endgenerate
endmodule
