// SPDX-License-Identifier: CERN-OHL-W-2.0
// Exercise both edges, reset values and priorities, and initial values.
module controls(input clk, en, rst, arst, input [7:0] d, output reg [7:0] q = 8'h96);
  always @(posedge clk) if (rst) q[0] <= 0; else if (en) q[0] <= d[0];
  always @(posedge clk) if (en) begin if (rst) q[1] <= 1; else q[1] <= d[1]; end
  always @(posedge clk or posedge arst) if (arst) q[2] <= 0; else if (en) q[2] <= d[2];
  always @(negedge clk or posedge arst) if (arst) q[3] <= 1; else if (en) q[3] <= d[3];
  always @(posedge clk or negedge arst) if (!arst) q[4] <= 1; else if (rst) q[4] <= 0; else if (en) q[4] <= d[4];
  always @(negedge clk) if (rst) q[5] <= 1; else if (en) q[5] <= d[5];
  always @(negedge clk) if (en) begin if (rst) q[6] <= 0; else q[6] <= d[6]; end
  always @(posedge clk) q[7] <= d[7];
endmodule
