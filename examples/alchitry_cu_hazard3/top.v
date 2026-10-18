// SPDX-License-Identifier: CERN-OHL-W-2.0
// Hazard3 (5-stage pipelined RV32I) on the Alchitry Cu (iCE40 HX8K, 100 MHz
// board clock, 12.5 MHz core clock): 8 KiB of block RAM with firmware, a
// UART (115200 8N1 on the USB serial port) and the eight LEDs.
//
// Memory map:
//   0x00000000-0x00001fff  RAM (code, data, stack)
//   0x40000000             UART: write = send a byte, read bit 0 = busy
//   0x40000004             LEDs: write
module top(input clk, input rst_n, output reg [7:0] led, output tx);
  // 12.5 MHz core clock.
  reg [2:0] div = 0;
  always @(posedge clk) div <= div + 1;
  wire cclk = div[2];

  // Reset for 16 core cycles after configuration or the reset button.
  reg [4:0] rst_cnt = 0;
  wire resetn = rst_cnt[4];
  always @(posedge cclk)
    if (!rst_n) rst_cnt <= 0;
    else if (!resetn) rst_cnt <= rst_cnt + 1;

  wire [31:0] haddr, hwdata;
  wire        hwrite;
  wire [1:0]  htrans;
  wire [2:0]  hsize;
  wire        hready;
  wire [31:0] hrdata;
  wire        pwrup_req, unblock_out;

  hazard3_cpu_1port #(
    .RESET_VECTOR(32'h0), .MTVEC_INIT(32'h0),
    .EXTENSION_A(0), .EXTENSION_C(0), .EXTENSION_M(0),
    .CSR_COUNTER(0)
  ) cpu (
    .clk(cclk), .clk_always_on(cclk), .rst_n(resetn),
    .pwrup_req(pwrup_req), .pwrup_ack(pwrup_req), .clk_en(), .unblock_out(unblock_out),
    .unblock_in(unblock_out),
    .haddr(haddr), .hwrite(hwrite), .htrans(htrans), .hsize(hsize), .hburst(), .hprot(),
    .hmastlock(), .hmaster(), .hexcl(), .hready(hready), .hresp(1'b0), .hexokay(1'b1),
    .hwdata(hwdata), .hrdata(hrdata),
    .fence_i_vld(), .fence_d_vld(), .fence_rdy(1'b1),
    .dbg_req_halt(1'b0), .dbg_req_halt_on_reset(1'b0), .dbg_req_resume(1'b0),
    .dbg_halted(), .dbg_running(), .dbg_data0_rdata(32'h0), .dbg_data0_wdata(),
    .dbg_data0_wen(), .dbg_instr_data(32'h0), .dbg_instr_data_vld(1'b0),
    .dbg_instr_data_rdy(), .dbg_instr_caught_exception(), .dbg_instr_caught_ebreak(),
    .dbg_sbus_addr(32'h0), .dbg_sbus_write(1'b0), .dbg_sbus_size(2'h0), .dbg_sbus_vld(1'b0),
    .dbg_sbus_rdy(), .dbg_sbus_err(), .dbg_sbus_wdata(32'h0), .dbg_sbus_rdata(),
    .mhartid_val(32'h0), .eco_version(4'h0),
    .irq(1'b0), .soft_irq(1'b0), .timer_irq(1'b0)
  );

  // AHB-Lite data phase.
  reg        dph_valid = 0, dph_write = 0, rd_wait = 0;
  reg [31:0] dph_addr = 0;
  reg [2:0]  dph_size = 0;
  always @(posedge cclk)
    if (!resetn) begin
      dph_valid <= 0;
      rd_wait <= 0;
    end else if (hready) begin
      dph_valid <= htrans[1];
      dph_write <= hwrite;
      dph_addr <= haddr;
      dph_size <= hsize;
      rd_wait <= htrans[1] && !hwrite;
    end else begin
      rd_wait <= 0;
    end
  // Reads take one wait state: the RAM reads the data phase address.
  assign hready = !rd_wait;

  wire is_io = dph_addr[30];
  wire [3:0] be = dph_size == 3'd0 ? 4'b0001 << dph_addr[1:0] :
                  dph_size == 3'd1 ? (dph_addr[1] ? 4'b1100 : 4'b0011) : 4'b1111;
  wire wr = dph_valid && dph_write && hready;

  reg [31:0] mem [0:2047];
  initial $readmemh("fw.hex", mem);
  reg [31:0] ram_q;
  always @(posedge cclk) begin
    ram_q <= mem[dph_addr[12:2]];
    if (wr && !is_io) begin
      if (be[0]) mem[dph_addr[12:2]][7:0] <= hwdata[7:0];
      if (be[1]) mem[dph_addr[12:2]][15:8] <= hwdata[15:8];
      if (be[2]) mem[dph_addr[12:2]][23:16] <= hwdata[23:16];
      if (be[3]) mem[dph_addr[12:2]][31:24] <= hwdata[31:24];
    end
  end

  // UART transmitter, 12.5 MHz / 109 = 114679 baud (-0.45%).
  localparam BAUD_DIV = 109;
  reg [9:0] tx_shift = 10'h3ff;
  reg [3:0] tx_bits = 0;
  reg [7:0] tx_div = 0;
  wire tx_busy = tx_bits != 0;
  assign tx = tx_shift[0];
  always @(posedge cclk) begin
    if (tx_busy) begin
      if (tx_div == BAUD_DIV - 1) begin
        tx_div <= 0;
        tx_shift <= {1'b1, tx_shift[9:1]};
        tx_bits <= tx_bits - 1;
      end else begin
        tx_div <= tx_div + 1;
      end
    end
    if (wr && is_io && !dph_addr[2] && !tx_busy) begin
      tx_shift <= {1'b1, hwdata[7:0], 1'b0};
      tx_bits <= 10;
      tx_div <= 0;
    end
    if (wr && is_io && dph_addr[2]) led <= hwdata[7:0];
  end

  assign hrdata = is_io ? {31'h0, tx_busy} : ram_q;
endmodule
