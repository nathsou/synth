#if defined(__GNUC__)
#pragma GCC diagnostic ignored "-Wunused-function"
#endif

/* Helpers (synth-sim C backend). Values are two-state, least significant
 * word first, with the bits above their width at 0. */

static inline uint64_t sim_mask(unsigned w) { return w >= 64 ? ~0ull : (1ull << w) - 1; }

static inline int64_t sim_sx(uint64_t x, unsigned w) {
    return w == 0 ? 0 : (int64_t)(x << (64 - w)) >> (64 - w);
}

/* Signed division and remainder of w-bit values; 0 on division by zero. */
static inline uint64_t sim_divs(uint64_t a, uint64_t b, unsigned w) {
    int64_t x = sim_sx(a, w), y = sim_sx(b, w);
    if (y == 0) return 0;
    if (y == -1) return (0 - (uint64_t)x) & sim_mask(w);
    return (uint64_t)(x / y) & sim_mask(w);
}

static inline uint64_t sim_mods(uint64_t a, uint64_t b, unsigned w) {
    int64_t x = sim_sx(a, w), y = sim_sx(b, w);
    if (y == 0 || y == -1) return 0;
    return (uint64_t)(x % y) & sim_mask(w);
}

/* Bits pos..pos + w (w <= 64) of the aw-bit value a (na words); bits
 * outside it read 0. */
static inline uint64_t sim_extract(const uint64_t *a, unsigned na, unsigned aw, int64_t pos, unsigned w) {
    if (w == 0 || pos >= (int64_t)aw || pos + (int64_t)w <= 0) return 0;
    if (pos >= 0) {
        uint64_t p = (uint64_t)pos;
        unsigned wi = (unsigned)(p / 64), sh = (unsigned)(p % 64);
        uint64_t r = a[wi] >> sh;
        int64_t room = (int64_t)aw - pos;
        if (sh && wi + 1 < na) r |= a[wi + 1] << (64 - sh);
        return r & sim_mask(room < (int64_t)w ? (unsigned)room : w);
    } else {
        unsigned s = (unsigned)(-pos);
        int64_t top = (int64_t)aw + s;
        return (a[0] << s) & sim_mask(top < (int64_t)w ? (unsigned)top : w) & ~sim_mask(s);
    }
}

/* ---- wide values ---- */

#define SIM_N(w) ((w) ? ((w) + 63u) / 64u : 1u)

/* d (dw bits) = t (tw bits), zero-extended or truncated. */
static void sim_wset(uint64_t *d, unsigned dw, const uint64_t *t, unsigned tw) {
    unsigned i, nd = SIM_N(dw), nt = SIM_N(tw);
    for (i = 0; i < nd; i++) d[i] = i < nt ? t[i] : 0;
    d[nd - 1] &= sim_mask(dw - (nd - 1) * 64);
}

/* t (w bits) = a (aw bits), zero-extended or truncated. */
static void sim_wget(uint64_t *t, unsigned w, const uint64_t *a, unsigned aw) { sim_wset(t, w, a, aw); }

static int sim_wzero(const uint64_t *a, unsigned w) {
    unsigned i;
    for (i = 0; i < SIM_N(w); i++)
        if (a[i]) return 0;
    return 1;
}

static int sim_wbit(const uint64_t *a, unsigned i) { return (int)(a[i / 64] >> (i % 64) & 1); }

static int sim_wcmpu(const uint64_t *a, const uint64_t *b, unsigned w) {
    unsigned i = SIM_N(w);
    while (i--)
        if (a[i] != b[i]) return a[i] < b[i] ? -1 : 1;
    return 0;
}

static int sim_wcmps(const uint64_t *a, const uint64_t *b, unsigned w) {
    int sa = w ? sim_wbit(a, w - 1) : 0, sb = w ? sim_wbit(b, w - 1) : 0;
    if (sa != sb) return sa ? -1 : 1;
    return sim_wcmpu(a, b, w);
}

static void sim_wadd_(uint64_t *t, const uint64_t *a, const uint64_t *b, unsigned w, int sub) {
    unsigned i, n = SIM_N(w);
    uint64_t c = sub;
    for (i = 0; i < n; i++) {
        uint64_t y = sub ? ~b[i] : b[i], s = a[i] + y;
        uint64_t c1 = s < a[i];
        t[i] = s + c;
        c = c1 | (t[i] < s);
    }
    t[n - 1] &= sim_mask(w - (n - 1) * 64);
}

static void sim_wneg_(uint64_t *t, const uint64_t *a, unsigned w) {
    uint64_t z[SIM_N(w)];
    unsigned i;
    for (i = 0; i < SIM_N(w); i++) z[i] = 0;
    sim_wadd_(t, z, a, w, 1);
}

static void sim_wshl_(uint64_t *t, const uint64_t *a, unsigned w, uint64_t n) {
    unsigned i, nw = SIM_N(w);
    for (i = 0; i < nw; i++) t[i] = 0;
    if (n >= w) return;
    for (i = 0; i + n < w; i++)
        if (sim_wbit(a, i)) t[(i + n) / 64] |= 1ull << ((i + n) % 64);
}

static void sim_wshr_(uint64_t *t, const uint64_t *a, unsigned w, uint64_t n, int arith) {
    unsigned i, nw = SIM_N(w);
    int fill = arith && w ? sim_wbit(a, w - 1) : 0;
    for (i = 0; i < nw; i++) t[i] = 0;
    for (i = 0; i < w; i++) {
        int b = i + n < w ? sim_wbit(a, (unsigned)(i + n)) : fill;
        if (b) t[i / 64] |= 1ull << (i % 64);
    }
}

/* Unsigned division with remainder of w-bit values (b nonzero). */
static void sim_wdivrem_(uint64_t *q, uint64_t *r, const uint64_t *a, const uint64_t *b, unsigned w) {
    unsigned i, n = SIM_N(w);
    uint64_t t[SIM_N(w)];
    for (i = 0; i < n; i++) q[i] = r[i] = 0;
    i = w;
    while (i--) {
        sim_wshl_(t, r, w, 1);
        t[0] |= (uint64_t)sim_wbit(a, i);
        if (sim_wcmpu(t, b, w) >= 0) {
            sim_wadd_(r, t, b, w, 1);
            q[i / 64] |= 1ull << (i % 64);
        } else {
            unsigned j;
            for (j = 0; j < n; j++) r[j] = t[j];
        }
    }
}

static uint64_t sim_wamount(const uint64_t *b, unsigned bw) {
    unsigned i;
    for (i = 1; i < SIM_N(bw); i++)
        if (b[i]) return ~0ull;
    return b[0];
}

#define SIM_WARGS uint64_t *d, unsigned dw, const uint64_t *a, unsigned aw, const uint64_t *b, unsigned bw

static void sim_wnot(SIM_WARGS) {
    uint64_t t[SIM_N(aw)];
    unsigned i;
    (void)b, (void)bw;
    for (i = 0; i < SIM_N(aw); i++) t[i] = ~a[i];
    t[SIM_N(aw) - 1] &= sim_mask(aw - (SIM_N(aw) - 1) * 64);
    sim_wset(d, dw, t, aw);
}

static void sim_wneg(SIM_WARGS) {
    uint64_t t[SIM_N(aw)];
    (void)b, (void)bw;
    sim_wneg_(t, a, aw);
    sim_wset(d, dw, t, aw);
}

static void sim_w1(uint64_t *d, unsigned dw, int x) {
    uint64_t t = (uint64_t)(x != 0);
    sim_wset(d, dw, &t, 1);
}

static void sim_wredor(SIM_WARGS) { (void)b, (void)bw; sim_w1(d, dw, !sim_wzero(a, aw)); }
static void sim_wlognot(SIM_WARGS) { (void)b, (void)bw; sim_w1(d, dw, sim_wzero(a, aw)); }

static void sim_wredand(SIM_WARGS) {
    unsigned i;
    int r = 1;
    (void)b, (void)bw;
    for (i = 0; i < aw; i++) r &= sim_wbit(a, i);
    sim_w1(d, dw, r);
}

static int sim_wparity(const uint64_t *a, unsigned w) {
    unsigned i, p = 0;
    for (i = 0; i < SIM_N(w); i++) p ^= (unsigned)__builtin_parityll(a[i]);
    return (int)p;
}

static void sim_wredxor(SIM_WARGS) { (void)b, (void)bw; sim_w1(d, dw, sim_wparity(a, aw)); }
static void sim_wredxnor(SIM_WARGS) { (void)b, (void)bw; sim_w1(d, dw, !sim_wparity(a, aw)); }

#define SIM_BITWISE(name, expr)                                        \
    static void name(SIM_WARGS) {                                      \
        uint64_t x[SIM_N(aw)], y[SIM_N(aw)], t[SIM_N(aw)];             \
        unsigned i;                                                    \
        sim_wget(x, aw, a, aw);                                        \
        sim_wget(y, aw, b, bw);                                        \
        for (i = 0; i < SIM_N(aw); i++) t[i] = (expr);                 \
        t[SIM_N(aw) - 1] &= sim_mask(aw - (SIM_N(aw) - 1) * 64);       \
        sim_wset(d, dw, t, aw);                                        \
    }
SIM_BITWISE(sim_wand, x[i] & y[i])
SIM_BITWISE(sim_wor, x[i] | y[i])
SIM_BITWISE(sim_wxor, x[i] ^ y[i])
SIM_BITWISE(sim_wxnor, ~(x[i] ^ y[i]))

static void sim_waddsub(SIM_WARGS, int sub) {
    uint64_t y[SIM_N(aw)], t[SIM_N(aw)];
    sim_wget(y, aw, b, bw);
    sim_wadd_(t, a, y, aw, sub);
    sim_wset(d, dw, t, aw);
}
static void sim_wadd(SIM_WARGS) { sim_waddsub(d, dw, a, aw, b, bw, 0); }
static void sim_wsub(SIM_WARGS) { sim_waddsub(d, dw, a, aw, b, bw, 1); }

static void sim_wmul(SIM_WARGS) {
    unsigned n = SIM_N(aw), i, j;
    uint64_t y[SIM_N(aw)], t[SIM_N(aw)];
    sim_wget(y, aw, b, bw);
    for (i = 0; i < n; i++) t[i] = 0;
    for (i = 0; i < n; i++) {
        unsigned __int128 c = 0;
        for (j = 0; i + j < n; j++) {
            unsigned __int128 cur = (unsigned __int128)t[i + j] + (unsigned __int128)a[i] * y[j] + c;
            t[i + j] = (uint64_t)cur;
            c = cur >> 64;
        }
    }
    t[n - 1] &= sim_mask(aw - (n - 1) * 64);
    sim_wset(d, dw, t, aw);
}

static void sim_wdivmod(SIM_WARGS, int sgn, int rem) {
    unsigned n = SIM_N(aw);
    uint64_t x[SIM_N(aw)], y[SIM_N(aw)], q[SIM_N(aw)], r[SIM_N(aw)];
    int na = sgn && aw && sim_wbit(a, aw - 1), nb;
    unsigned i;
    sim_wget(x, aw, a, aw);
    sim_wget(y, aw, b, bw);
    nb = sgn && aw && sim_wbit(y, aw - 1);
    if (sim_wzero(y, aw)) {
        for (i = 0; i < n; i++) q[i] = 0;
        sim_wset(d, dw, q, aw);
        return;
    }
    if (na) sim_wneg_(x, x, aw);
    if (nb) sim_wneg_(y, y, aw);
    sim_wdivrem_(q, r, x, y, aw);
    if (rem) {
        if (na) sim_wneg_(r, r, aw);
        sim_wset(d, dw, r, aw);
    } else {
        if (na != nb) sim_wneg_(q, q, aw);
        sim_wset(d, dw, q, aw);
    }
}
static void sim_wdivu(SIM_WARGS) { sim_wdivmod(d, dw, a, aw, b, bw, 0, 0); }
static void sim_wmodu(SIM_WARGS) { sim_wdivmod(d, dw, a, aw, b, bw, 0, 1); }
static void sim_wdivs(SIM_WARGS) { sim_wdivmod(d, dw, a, aw, b, bw, 1, 0); }
static void sim_wmods(SIM_WARGS) { sim_wdivmod(d, dw, a, aw, b, bw, 1, 1); }

static void sim_wshl(SIM_WARGS) {
    uint64_t t[SIM_N(aw)];
    sim_wshl_(t, a, aw, sim_wamount(b, bw));
    sim_wset(d, dw, t, aw);
}
static void sim_wshr(SIM_WARGS) {
    uint64_t t[SIM_N(aw)];
    sim_wshr_(t, a, aw, sim_wamount(b, bw), 0);
    sim_wset(d, dw, t, aw);
}
static void sim_wsshr(SIM_WARGS) {
    uint64_t t[SIM_N(aw)];
    sim_wshr_(t, a, aw, sim_wamount(b, bw), 1);
    sim_wset(d, dw, t, aw);
}

#define SIM_CMP(name, cmp, test)                                       \
    static void name(SIM_WARGS) {                                      \
        uint64_t y[SIM_N(aw)];                                         \
        sim_wget(y, aw, b, bw);                                        \
        sim_w1(d, dw, cmp(a, y, aw) test);                             \
    }
SIM_CMP(sim_weq, sim_wcmpu, == 0)
SIM_CMP(sim_wne, sim_wcmpu, != 0)
SIM_CMP(sim_wltu, sim_wcmpu, < 0)
SIM_CMP(sim_wleu, sim_wcmpu, <= 0)
SIM_CMP(sim_wgtu, sim_wcmpu, > 0)
SIM_CMP(sim_wgeu, sim_wcmpu, >= 0)
SIM_CMP(sim_wlts, sim_wcmps, < 0)
SIM_CMP(sim_wles, sim_wcmps, <= 0)
SIM_CMP(sim_wgts, sim_wcmps, > 0)
SIM_CMP(sim_wges, sim_wcmps, >= 0)

static void sim_wland(SIM_WARGS) { sim_w1(d, dw, !sim_wzero(a, aw) && !sim_wzero(b, bw)); }
static void sim_wlor(SIM_WARGS) { sim_w1(d, dw, !sim_wzero(a, aw) || !sim_wzero(b, bw)); }

/* d = a[b + offset +: dw], bits outside a reading 0. */
static void sim_wshiftx(uint64_t *d, unsigned dw, const uint64_t *a, unsigned aw, const uint64_t *b, unsigned bw,
                        unsigned b_signed, int64_t offset) {
    unsigned i, n = SIM_N(dw);
    int64_t idx;
    if (bw <= 64) {
        idx = b_signed ? sim_sx(b[0], bw) : (int64_t)b[0];
    } else {
        idx = sim_wamount(b, bw) == ~0ull ? INT64_MAX / 2 : (int64_t)b[0];
    }
    idx += offset;
    for (i = 0; i < n; i++) d[i] = 0;
    for (i = 0; i < dw; i++) {
        int64_t j = idx + i;
        if (j >= 0 && j < (int64_t)aw && sim_wbit(a, (unsigned)j)) d[i / 64] |= 1ull << (i % 64);
    }
}
