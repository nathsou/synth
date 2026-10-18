// SPDX-License-Identifier: CERN-OHL-W-2.0
module lut6(input [5:0] a, output y, z);
  assign y = 64'hb6d39a50e7c1842f >> a;
  assign z = a[0] ? ((a[1] & a[2]) ^ (a[3] | a[4])) : (a[5] ^ a[2]);
endmodule
