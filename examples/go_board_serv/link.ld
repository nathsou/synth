/* From SERV (https://github.com/olofk/serv, ISC licence). */
OUTPUT_ARCH( "riscv" )
ENTRY(_start)

SECTIONS
{
  . = 0x00000000;
  .text : { *(.text) }
  .data : { *(.data) }
  .bss : { *(.bss) }
}
