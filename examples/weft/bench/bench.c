// SPDX-License-Identifier: CERN-OHL-W-2.0
// A benchmark for the RISC-V cores: small kernels of the kinds of work
// embedded code does (checksums, sorting, sieving, pointer chasing, string
// hashing, calls and returns, multiplication in software, bit twiddling),
// each giving a checksum. The kernels run between two writes to the LEDs
// (1 at the start, 2 at the end), so that a simulation can count the cycles
// and instructions in between; then the checksums and the cycles counted by
// the system, if it has a counter, are printed in hexadecimal.

#include <stdint.h>

#ifdef HOST
// Built for the computer it runs on (cc -DHOST bench.c), for the expected
// output.
#include <stdio.h>
static uint32_t LEDS, CYCLES;
static void putc_(char c) { putchar(c); }
#else
#ifndef DEV_BASE
#define DEV_BASE 0x10000000u
#endif
#define LEDS (*(volatile uint32_t *)(DEV_BASE + 0))
#define UART (*(volatile uint32_t *)(DEV_BASE + 4))  // write: send; read: bit 0 is busy
#define CYCLES (*(volatile uint32_t *)(DEV_BASE + 8)) // read: cycles since configuration

static void putc_(char c) {
    while (UART & 1) {}
    UART = (uint8_t)c;
}
#endif

static void puts_(const char *s) {
    while (*s) putc_(*s++);
}

static void puthex(uint32_t v) {
    for (int i = 28; i >= 0; i -= 4) putc_("0123456789abcdef"[(v >> i) & 15]);
}

// rv32i has no multiplication or division: the compiler calls these.
uint32_t __mulsi3(uint32_t a, uint32_t b) {
    uint32_t r = 0;
    while (b) {
        if (b & 1) r += a;
        a <<= 1;
        b >>= 1;
    }
    return r;
}

static uint32_t udivmod(uint32_t n, uint32_t d, uint32_t *rem) {
    uint32_t q = 0, r = 0;
    for (int i = 31; i >= 0; i--) {
        r = (r << 1) | ((n >> i) & 1);
        if (r >= d) {
            r -= d;
            q |= 1u << i;
        }
    }
    *rem = r;
    return q;
}

uint32_t __udivsi3(uint32_t n, uint32_t d) {
    uint32_t r;
    return udivmod(n, d, &r);
}

uint32_t __umodsi3(uint32_t n, uint32_t d) {
    uint32_t r;
    udivmod(n, d, &r);
    return r;
}

void *memset(void *p, int c, unsigned n) {
    uint8_t *b = p;
    while (n--) *b++ = (uint8_t)c;
    return p;
}

void *memcpy(void *d, const void *s, unsigned n) {
    uint8_t *a = d;
    const uint8_t *b = s;
    while (n--) *a++ = *b++;
    return d;
}

static uint32_t rng = 0x12345678;

static uint32_t xorshift(void) {
    rng ^= rng << 13;
    rng ^= rng >> 17;
    rng ^= rng << 5;
    return rng;
}

// CRC-32 (reflected, polynomial 0xEDB88320), a bit at a time.
static uint32_t crc32(const uint8_t *p, unsigned n) {
    uint32_t crc = 0xffffffff;
    while (n--) {
        crc ^= *p++;
        for (int k = 0; k < 8; k++) crc = (crc >> 1) ^ (0xedb88320 & -(crc & 1));
    }
    return ~crc;
}

static uint8_t buf[256];

static uint32_t k_crc(void) {
    for (unsigned i = 0; i < sizeof buf; i++) buf[i] = (uint8_t)xorshift();
    return crc32(buf, sizeof buf);
}

// Quicksort of signed integers, then a check that they are in order.
static void quicksort(int32_t *a, int lo, int hi) {
    while (lo < hi) {
        int32_t p = a[(lo + hi) >> 1];
        int i = lo, j = hi;
        while (i <= j) {
            while (a[i] < p) i++;
            while (a[j] > p) j--;
            if (i <= j) {
                int32_t t = a[i];
                a[i] = a[j];
                a[j] = t;
                i++;
                j--;
            }
        }
        // Recurse into the smaller part, loop on the larger.
        if (j - lo < hi - i) {
            quicksort(a, lo, j);
            lo = i;
        } else {
            quicksort(a, i, hi);
            hi = j;
        }
    }
}

static int32_t arr[128];

static uint32_t k_sort(void) {
    const int n = sizeof arr / sizeof arr[0];
    for (int i = 0; i < n; i++) arr[i] = (int32_t)xorshift() >> 8;
    quicksort(arr, 0, n - 1);
    uint32_t sum = 0;
    for (int i = 0; i < n; i++) {
        if (i > 0 && arr[i - 1] > arr[i]) return 0xbad;
        sum = (sum << 3 | sum >> 29) ^ (uint32_t)arr[i];
    }
    return sum;
}

// The primes below 1200, sieved in bytes.
static uint8_t composite[1200];

static uint32_t k_sieve(void) {
    const unsigned n = sizeof composite;
    memset(composite, 0, n);
    uint32_t count = 0, sum = 0;
    for (unsigned i = 2; i < n; i++) {
        if (composite[i]) continue;
        count++;
        sum += i;
        for (unsigned j = i + i; j < n; j += i) composite[j] = 1;
    }
    return count << 20 ^ sum;
}

// A linked list in an array, in a shuffled order, walked a few times.
struct node {
    struct node *next;
    uint32_t value;
};

static struct node nodes[64];

static uint32_t k_list(void) {
    const unsigned n = sizeof nodes / sizeof nodes[0];
    uint8_t order[sizeof nodes / sizeof nodes[0]];
    for (unsigned i = 0; i < n; i++) order[i] = (uint8_t)i;
    for (unsigned i = n - 1; i > 0; i--) {
        unsigned j = xorshift() % (i + 1);
        uint8_t t = order[i];
        order[i] = order[j];
        order[j] = t;
    }
    for (unsigned i = 0; i < n; i++) {
        nodes[order[i]].next = i + 1 < n ? &nodes[order[i + 1]] : 0;
        nodes[order[i]].value = xorshift() & 0xffff;
    }
    uint32_t h = 0;
    for (int round = 0; round < 8; round++) {
        for (struct node *p = &nodes[order[0]]; p; p = p->next) h = h * 31 + p->value;
    }
    return h;
}

// A hash of the words of a text and a count of its vowels.
static const char text[] =
    "It was the best of times, it was the worst of times, it was the age of "
    "wisdom, it was the age of foolishness, it was the epoch of belief, it "
    "was the epoch of incredulity, it was the season of Light, it was the "
    "season of Darkness, it was the spring of hope, it was the winter of "
    "despair, we had everything before us, we had nothing before us.";

static uint32_t k_text(void) {
    uint32_t words = 0, vowels = 0, h = 0, wh = 2166136261u;
    for (int round = 0; round < 4; round++) {
        for (const char *s = text; *s; s++) {
            char c = *s;
            if (c >= 'A' && c <= 'Z') c += 'a' - 'A';
            if (c >= 'a' && c <= 'z') {
                wh = (wh ^ (uint8_t)c) * 16777619u;
                switch (c) {
                case 'a': case 'e': case 'i': case 'o': case 'u': vowels++; break;
                default: break;
                }
            } else if (wh != 2166136261u) {
                words++;
                h ^= wh;
                wh = 2166136261u;
            }
        }
    }
    return h ^ words << 16 ^ vowels;
}

// Calls and returns: a recursive sum over a binary tree of calls.
static uint32_t tree(uint32_t depth, uint32_t x) {
    if (depth == 0) return x ^ (x >> 3);
    return tree(depth - 1, x * 2 + 1) + tree(depth - 1, x * 2);
}

static uint32_t k_calls(void) {
    return tree(10, 1);
}

// A product of 8 x 8 matrices of small integers.
#define N 8
static int32_t ma[N][N], mb[N][N], mc[N][N];

static uint32_t k_matrix(void) {
    for (int i = 0; i < N; i++) {
        for (int j = 0; j < N; j++) {
            ma[i][j] = (int32_t)(xorshift() & 0xff) - 128;
            mb[i][j] = (int32_t)(xorshift() & 0xff) - 128;
        }
    }
    uint32_t sum = 0;
    for (int i = 0; i < N; i++) {
        for (int j = 0; j < N; j++) {
            int32_t s = 0;
            for (int k = 0; k < N; k++) s += ma[i][k] * mb[k][j];
            mc[i][j] = s;
            sum += (uint32_t)s ^ (uint32_t)(i << 8 | j);
        }
    }
    return sum;
}

// Bit twiddling: population counts, bit reversal and rotations.
static uint32_t k_bits(void) {
    uint32_t acc = 0;
    for (int i = 0; i < 300; i++) {
        uint32_t x = xorshift();
        uint32_t c = 0;
        for (uint32_t y = x; y; y &= y - 1) c++;
        uint32_t r = 0;
        for (int k = 0; k < 32; k++) r = (r << 1) | ((x >> k) & 1);
        int32_t s = (int32_t)x >> (c & 31);
        acc = (acc << 7 | acc >> 25) ^ r ^ (uint32_t)s ^ c;
    }
    return acc;
}

typedef uint32_t (*kernel)(void);
static const kernel kernels[] = {k_crc, k_sort, k_sieve, k_list, k_text, k_calls, k_matrix, k_bits};
#define KERNELS (sizeof kernels / sizeof kernels[0])
static uint32_t results[KERNELS];

int main(void) {
    LEDS = 1;
    uint32_t start = CYCLES;
    for (unsigned k = 0; k < KERNELS; k++) results[k] = kernels[k]();
    uint32_t cycles = CYCLES - start;
    LEDS = 2;
    for (unsigned k = 0; k < KERNELS; k++) {
        puthex(results[k]);
        putc_(k + 1 < KERNELS ? ' ' : '\n');
    }
    puts_("cycles ");
    puthex(cycles);
    putc_('\n');
    LEDS = 3;
#ifdef HOST
    return 0;
#else
    for (;;) {}
#endif
}
