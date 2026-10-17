/* SPDX-License-Identifier: CERN-OHL-W-2.0 */
	.section .text.start
	.globl _start
_start:
	la	sp, _stack_top
	/* Clear .bss */
	la	a0, _bss_start
	la	a1, _bss_end
1:	bgeu	a0, a1, 2f
	sw	zero, 0(a0)
	addi	a0, a0, 4
	j	1b
2:	call	main
3:	j	3b
