// SPDX-License-Identifier: CERN-OHL-W-2.0
// Test design: counter-driven LEDs through an initialized, writable block RAM.
module top(input clk, input btn, output [7:0] led, output tx);
  reg [23:0] cnt = 0;
  reg [7:0] mem [0:511];
  integer i;
  initial for (i = 0; i < 512; i = i + 1) mem[i] = i * 7 + 3;
  reg [7:0] q;
  always @(posedge clk) begin
    cnt <= cnt + (btn ? 2 : 1);
    q <= mem[cnt[23:15]];
    if (btn) mem[cnt[8:0]] <= cnt[16:9];
  end
  assign led = q ^ cnt[23:16];
  assign tx = ^cnt;
endmodule
