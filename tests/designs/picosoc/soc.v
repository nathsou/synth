// Minimal PicoRV32 SoC: 4 KiB BRAM (code + data), UART TX, LEDs.
module top(input clk, input btn, output reg [7:0] led, output tx);
  reg [5:0] rst_cnt = 0;
  wire resetn = &rst_cnt;
  always @(posedge clk) if (!resetn) rst_cnt <= rst_cnt + 1;

  wire mem_valid, mem_instr;
  reg mem_ready;
  wire [31:0] mem_addr, mem_wdata;
  wire [3:0] mem_wstrb;
  reg [31:0] mem_rdata;

  picorv32 #(
    .ENABLE_COUNTERS(0), .ENABLE_COUNTERS64(0), .BARREL_SHIFTER(0), .TWO_STAGE_SHIFT(0),
    .CATCH_MISALIGN(0), .CATCH_ILLINSN(0), .COMPRESSED_ISA(0), .ENABLE_MUL(0), .ENABLE_DIV(0),
    .PROGADDR_RESET(32'h0), .STACKADDR(32'h1000)
  ) cpu (
    .clk(clk), .resetn(resetn), .mem_valid(mem_valid), .mem_instr(mem_instr), .mem_ready(mem_ready),
    .mem_addr(mem_addr), .mem_wdata(mem_wdata), .mem_wstrb(mem_wstrb), .mem_rdata(mem_rdata)
  );

  reg [31:0] mem [0:1023];
  initial $readmemh("fw.hex", mem);

  // UART: 8N1, 8 clocks per bit.
  reg [9:0] tx_shift = 10'h3ff;
  reg [3:0] tx_bits = 0;
  reg [2:0] tx_div = 0;
  assign tx = tx_shift[0];
  wire tx_busy = tx_bits != 0;

  wire is_mem = mem_addr[31] == 1'b0;
  always @(posedge clk) begin
    mem_ready <= 0;
    if (tx_busy) begin
      tx_div <= tx_div + 1;
      if (tx_div == 7) begin
        tx_shift <= {1'b1, tx_shift[9:1]};
        tx_bits <= tx_bits - 1;
      end
    end
    if (mem_valid && !mem_ready) begin
      if (is_mem) begin
        mem_rdata <= mem[mem_addr[11:2]];
        if (mem_wstrb[0]) mem[mem_addr[11:2]][7:0] <= mem_wdata[7:0];
        if (mem_wstrb[1]) mem[mem_addr[11:2]][15:8] <= mem_wdata[15:8];
        if (mem_wstrb[2]) mem[mem_addr[11:2]][23:16] <= mem_wdata[23:16];
        if (mem_wstrb[3]) mem[mem_addr[11:2]][31:24] <= mem_wdata[31:24];
        mem_ready <= 1;
      end else begin
        case (mem_addr[3:2])
          2'd0: if (mem_wstrb != 0 && !tx_busy) begin
                  tx_shift <= {1'b1, mem_wdata[7:0], 1'b0};
                  tx_bits <= 10;
                  tx_div <= 0;
                end
          2'd2: if (mem_wstrb != 0) led <= mem_wdata[7:0];
          default: ;
        endcase
        mem_rdata <= {30'b0, btn, tx_busy};
        mem_ready <= 1;
      end
    end
  end
endmodule
