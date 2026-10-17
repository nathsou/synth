/* SPDX-License-Identifier: CERN-OHL-W-2.0
 * Prints a line over a bit-banged UART on servant's GPIO, forever.
 * Bit time: BIT_NOPS delay instructions per bit, calibrated in simulation
 * for 19200 baud with a 25 MHz clock (1287 cycles per bit, +1.2%).
 */
#define GPIO_ADDR 0x40000000
#ifndef BIT_NOPS
#define BIT_NOPS 30
#endif

.globl _start
_start:
	li	a0, GPIO_ADDR
	addi	t0, zero, 1
	sb	t0, 0(a0)		/* idle high */
line:
	la	a1, str
next_char:
	lbu	t0, 0(a1)
	beqz	t0, pause
	ori	t0, t0, 0x100		/* stop bit */
	slli	t0, t0, 1		/* start bit */
1:	sb	t0, 0(a0)
	srli	t0, t0, 1
	.rept BIT_NOPS
	nop
	.endr
	bnez	t0, 1b
	addi	a1, a1, 1
	j	next_char
pause:	/* about a second */
	li	t1, 300000
2:	addi	t1, t1, -1
	bnez	t1, 2b
	j	line

	.section .data
str:	.string "Hello from SERV, built by synth!\r\n"
