#define UART (*(volatile unsigned *)0x80000000)
#define STAT (*(volatile unsigned *)0x80000004)
#define LEDS (*(volatile unsigned *)0x80000008)

static void putc_(char c) { while (STAT & 1) ; UART = c; }
static void puts_(const char *s) { while (*s) putc_(*s++); }
static void puthex(unsigned v) { for (int i = 28; i >= 0; i -= 4) putc_("0123456789abcdef"[(v >> i) & 15]); }

static unsigned mul(unsigned a, unsigned b) { unsigned r = 0; while (b) { if (b & 1) r += a; a <<= 1; b >>= 1; } return r; }

int main(void) {
  puts_("Hello from synth!\n");
  // Sieve of Eratosthenes up to 400, plus a checksum.
  static unsigned char comp[400];
  unsigned count = 0, sum = 0;
  for (unsigned i = 2; i < 400; i++) {
    if (!comp[i]) { count++; sum = mul(sum, 31) + i; for (unsigned j = i + i; j < 400; j += i) comp[j] = 1; }
  }
  puts_("primes=");
  puthex(count);
  puts_(" sum=");
  puthex(sum);
  putc_('\n');
  for (unsigned n = 0;; n++) { LEDS = n >> 4; }
}
