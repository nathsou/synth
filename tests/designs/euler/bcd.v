// BCD arithmetic for the Project Euler solvers.
// Ported from HDLean (https://github.com/nathsou/hdlean, Designs/Euler/Bcd.lean).

// Adds two N-digit BCD numbers (and, with CARRY_IN, a carry): N + 1 digits.
// Every digit's sum, generate (>= 10) and propagate (>= 9) are computed in
// parallel and the decimal carries resolved by one binary carry chain:
// carries = ((P + G) ^ P ^ G). The carry in enters the chain as an extra
// lowest position.
module euler_bcd_add #(
  parameter N = 1,
  parameter CARRY_IN = 0
) (
  input wire [4*N-1:0] a,
  input wire [4*N-1:0] b,
  input wire cin,
  output wire [4*N+3:0] s
);
  localparam C = CARRY_IN ? 1 : 0;
  wire [5*N-1:0] sums;
  wire [N+C:0] p, g;
  genvar i;
  generate
    if (C) begin : with_cin
      assign p[0] = cin;
      assign g[0] = cin;
    end
    assign p[N+C] = 1'b0;
    assign g[N+C] = 1'b0;
    for (i = 0; i < N; i = i + 1) begin : digit
      assign sums[5*i +: 5] = {1'b0, a[4*i +: 4]} + {1'b0, b[4*i +: 4]};
      assign g[i+C] = sums[5*i +: 5] >= 5'd10;
      assign p[i+C] = sums[5*i +: 5] >= 5'd9;
    end
  endgenerate
  wire [N+C:0] carries = ((p + g) ^ p) ^ g;
  generate
    for (i = 0; i < N; i = i + 1) begin : out
      wire [4:0] t = sums[5*i +: 5] + {4'd0, carries[i+C]};
      wire [4:0] fixed = t + (carries[i+C+1] ? 5'd6 : 5'd0);
      assign s[4*i +: 4] = fixed[3:0];
    end
  endgenerate
  assign s[4*N +: 4] = {3'd0, carries[N+C]};
endmodule

// Doubles an N-digit BCD number: N + 1 digits, without a carry chain.
module euler_bcd_double #(
  parameter N = 1
) (
  input wire [4*N-1:0] a,
  output wire [4*N+3:0] s
);
  genvar i;
  generate
    for (i = 0; i <= N; i = i + 1) begin : digit
      wire [3:0] low;
      wire carry;
      if (i < N) begin : l
        wire [3:0] d = a[4*i +: 4];
        assign low = d < 5 ? {d[2:0], 1'b0} : {d[2:0], 1'b0} - 4'd10;
      end else begin : z
        assign low = 4'd0;
      end
      if (i > 0) begin : c
        assign carry = a[4*(i-1) +: 4] >= 4'd5;
      end else begin : nc
        assign carry = 1'b0;
      end
      assign s[4*i +: 4] = low | {3'd0, carry};
    end
  endgenerate
endmodule
