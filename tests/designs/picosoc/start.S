.section .text.start
.global _start
_start:
  li sp, 0x1000
  call main
1: j 1b
