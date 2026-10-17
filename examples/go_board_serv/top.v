// SPDX-License-Identifier: CERN-OHL-W-2.0
// SERV (bit-serial RV32I) on the Nandland Go Board (iCE40 HX1K, 25 MHz):
// prints a line over the USB UART (115200 8N1) once a second.
// LED1 lights while a character is being sent; LED4 blinks.
module top(input clk, output [3:0] led, output tx);
  // Reset for 16 cycles after configuration.
  reg [4:0] rst_cnt = 0;
  wire rst = !rst_cnt[4];
  always @(posedge clk) if (rst) rst_cnt <= rst_cnt + 1;

  wire q;
  servant #(.memfile("hello.hex"), .memsize(4096)) soc (.wb_clk(clk), .wb_rst(rst), .q(q));

  reg [24:0] beat = 0;
  always @(posedge clk) beat <= beat + 1;

  assign tx = q;
  assign led = {beat[24], 2'b00, ~q};
endmodule
