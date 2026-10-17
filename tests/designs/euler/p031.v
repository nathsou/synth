// Project Euler 31: the number of ways to make 200p from the coins 1p, 2p,
// 5p, 10p, 20p, 50p, 100p and 200p.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/P031.lean).
//
// The dynamic programme ways[i] += ways[i - c], one coin after another, as a
// systolic pipeline: one stage per coin, the amounts 0 ... 200 streamed through
// the stages one per cycle. Stage c adds to each incoming count its own output
// from c amounts earlier: a register for the 1p and 2p coins, a block RAM
// delay line for the others (written from the stage's output register, read
// two cycles ahead).
module euler31 (
  input wire clk,
  input wire rst,
  input wire start,
  output reg done,
  output reg [16:0] answer
);
  localparam W = 17;
  localparam STAGES = 8;
  localparam [7:0] TARGET = 8'd200;

  function integer coin(input integer s);
    case (s)
      0: coin = 1; 1: coin = 2; 2: coin = 5; 3: coin = 10;
      4: coin = 20; 5: coin = 50; 6: coin = 100; default: coin = 200;
    endcase
  endfunction

  reg [7:0] gen;
  reg fin;
  wire running = !(gen >= TARGET + 8'd1);

  // stage inputs: from the generator, then from the previous stage's registers
  wire vin [0:STAGES];
  wire [7:0] iin [0:STAGES];
  wire [W-1:0] xin [0:STAGES];
  assign vin[0] = running;
  assign iin[0] = gen;
  assign xin[0] = {{(W-1){1'b0}}, gen == 0};

  genvar s;
  generate
    for (s = 0; s < STAGES; s = s + 1) begin : stage
      localparam integer C = coin(s);
      reg v;
      reg [7:0] i = 0;
      reg [W-1:0] y = 0, g = 0;
      assign vin[s+1] = v;
      assign iin[s+1] = i;
      assign xin[s+1] = y;
      wire has = vin[s] && iin[s] >= C - 1;
      always @(posedge clk)
        if (rst) v <= 0;
        else v <= vin[s];
      if (C <= 2) begin : reg_line
        // h is a flag, registered a cycle ahead: iin >= c
        reg h;
        wire [W-1:0] prev = C == 1 ? y : g;
        always @(posedge clk) begin
          if (rst) h <= 0;
          else h <= has;
          i <= iin[s];
          y <= xin[s] + (h ? prev : {W{1'b0}});
          g <= y;
        end
      end else begin : ram_line
        reg [W-1:0] hist [0:255];
        reg [7:0] ra = 0;
        reg [W-1:0] h = 0;
        always @(posedge clk) begin
          if (v) hist[i] <= y;
          h <= hist[ra];
          i <= iin[s];
          y <= xin[s] + g;
          g <= has ? h : {W{1'b0}};
          ra <= iin[s] + 8'd3 - C;
        end
      end
    end
  endgenerate

  always @(posedge clk)
    if (rst) begin
      gen <= 0;
      fin <= 0;
      done <= 0;
      answer <= 0;
    end else begin
      if (running) gen <= gen + 8'd1;
      // the last stage takes amount 200 this cycle: its output is the answer next cycle
      fin <= vin[STAGES-1] && iin[STAGES-1] == TARGET;
      if (fin) answer <= xin[STAGES];
      done <= done || fin;
    end
endmodule
