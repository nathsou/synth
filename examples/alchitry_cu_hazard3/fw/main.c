// SPDX-License-Identifier: CERN-OHL-W-2.0
#include <stdint.h>

#define UART (*(volatile uint32_t *)0x40000000)
#define LEDS (*(volatile uint32_t *)0x40000004)

static void putc_(char c) {
	while (UART & 1)
		;
	UART = (uint8_t)c;
}

static void puts_(const char *s) {
	while (*s)
		putc_(*s++);
}

static void put_hex(uint32_t v) {
	for (int i = 28; i >= 0; i -= 4)
		putc_("0123456789abcdef"[(v >> i) & 15]);
}

int main(void) {
	uint32_t n = 0;
	for (;;) {
		puts_("Hello from Hazard3 (5-stage RV32I), built by synth! count=0x");
		put_hex(n);
		puts_("\r\n");
		LEDS = n;
		n++;
		for (volatile uint32_t d = 0; d < 300000; d++)
			;
	}
}
