module top(input clk, input btn, output [7:0] led, output tx);
reg [15:0] lfsr=16'hace1;
always @(posedge clk) lfsr <= {lfsr[14:0],lfsr[15]^lfsr[13]^lfsr[12]^lfsr[10]^btn};
reg [4:0] add=0, sub=0, twice=0;
reg [3:0] cin_sum=0;
reg [8:0] repeated=0;
reg [3:0] counter=0;
always @(posedge clk) begin
 add <= {1'b0,lfsr[3:0]} + 5'd5;
 sub <= {1'b0,lfsr[3:0]} - 5'd3;
 twice <= {1'b0,lfsr[3:0]} + {1'b0,lfsr[3:0]};
 cin_sum <= {1'b0,lfsr[2:0]} + 4'd5 + btn;
 repeated <= {1'b0,{8{lfsr[0]}}} + 9'd5;
 counter <= counter + 1'b1;
end
assign led = {add[3:0],sub[3:0]} ^ {twice[3:0],cin_sum} ^ repeated[7:0] ^ {counter,counter};
assign tx = add[4] ^ sub[4] ^ twice[4] ^ repeated[8];
endmodule
