module multiplier #(parameter A=24, B=17, Y=48, SIGNED=0)(
    input [A-1:0] a, input [B-1:0] b, output [Y-1:0] y
);
    generate if (SIGNED) begin
        wire signed [A-1:0] sa=a;
        wire signed [B-1:0] sb=b;
        assign y=sa*sb;
    end else begin
        assign y=a*b;
    end endgenerate
endmodule
