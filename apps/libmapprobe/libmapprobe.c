#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// Runtime proof of the shared library cache (docs/ai/pager/library-cache.md).
//
// Armed only by `cargo xtask kvm-smoke --library-map-probe`, which writes the
// private contract below. Without it this session-startup program exits at once
// and silently, so an ordinary boot performs no probe activity.
//
// It maps a library that pagerd warmed, from two processes, and proves with a
// read-only kernel diagnostic (`SYS_RUSTOS_LIBRARY_MAP_PROBE`, honoured only for
// this executable's exact path) that:
//   - the mapping was served by aliasing sealed frames (alias counters moved);
//   - both processes, and a fork-inherited copy, sit on the same physical
//     frames, and the bytes hash to the manifest digest;
//   - the tail of the last page beyond EOF reads as zero;
//   - a private write copies one page and leaves the other process on the
//     original frame with original bytes;
//   - munmap and exit return the leaf and frame counts.

#define SYS_RUSTOS_DEBUG_PRINT 0x52550001UL
#define SYS_RUSTOS_LIBRARY_MAP_PROBE 0x52550065UL
#define CONTRACT_PATH "/system/registry/system/libmapprobe-v1.env"
#define CONTRACT_BODY "contract=rustos-libmapprobe-v1\n"
#define MANIFEST_PATH "/etc/pagerd/library-cache.conf"
#define LIBRARY_PATH "/lib/x86_64-linux-gnu/libgcc_s.so.1"
#define PAGE 4096UL
#define MAX_PAGES 64U
#define WARM_ROWS 6U
#define WARM_DEADLINE_MS 60000UL
#define START_DELAY_MS 4000UL
#define FREE_TOLERANCE_PAGES 192
#define LEAF_TOLERANCE 8

struct probe_args { uint32_t op; uint32_t count; uint64_t addr; uint64_t out; uint64_t pid; };
struct frame { uint64_t phys; uint64_t flags; };
struct stats {
    uint64_t free_pages, own_data_leaves, sealed_objects, reserved_pages,
        alias_maps, alias_pages;
    uint64_t decline_counts[8], decline_details[8];
};

static void debug_line(const char *message) {
    size_t len = strlen(message);
    if (len != 0) {
        (void)syscall(SYS_RUSTOS_DEBUG_PRINT, message, len);
    }
    (void)syscall(SYS_RUSTOS_DEBUG_PRINT, "\n", 1UL);
}

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
#include <stdarg.h>
static void say(const char *fmt, ...) {
    char line[200];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(line, sizeof(line), fmt, ap);
    va_end(ap);
    if (n > 0 && (size_t)n < sizeof(line)) {
        debug_line(line);
    }
}

static int fail(const char *what, long a, long b) {
    say("library-map-probe: FAIL %s %ld %ld errno=%d", what, a, b, errno);
    return 1;
}

static uint64_t now_ms(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000UL + (uint64_t)now.tv_nsec / 1000000UL;
}

static void sleep_ms(long ms) {
    struct timespec pause = {.tv_sec = ms / 1000, .tv_nsec = (ms % 1000) * 1000000L};
    nanosleep(&pause, NULL);
}

static int frames_of_pid(uint64_t pid, const void *addr, uint32_t count, struct frame *out) {
    struct probe_args args = {1, count, (uint64_t)(uintptr_t)addr, (uint64_t)(uintptr_t)out, pid};
    return (int)syscall(SYS_RUSTOS_LIBRARY_MAP_PROBE, &args);
}

static int frames_of(const void *addr, uint32_t count, struct frame *out) {
    struct probe_args args = {1, count, (uint64_t)(uintptr_t)addr, (uint64_t)(uintptr_t)out, 0};
    return (int)syscall(SYS_RUSTOS_LIBRARY_MAP_PROBE, &args);
}

static int stats_of(struct stats *out) {
    struct probe_args args = {2, 0, 0, (uint64_t)(uintptr_t)out, 0};
    return (int)syscall(SYS_RUSTOS_LIBRARY_MAP_PROBE, &args);
}

// ---- SHA-256 ---------------------------------------------------------------
struct sha { uint32_t h[8]; uint8_t block[64]; uint64_t bytes; };
static const uint32_t K[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2};
#define ROR(x, n) (((x) >> (n)) | ((x) << (32 - (n))))
static void sha_block(struct sha *s, const uint8_t *p) {
    uint32_t w[64];
    for (int i = 0; i < 16; i++) {
        w[i] = (uint32_t)p[i * 4] << 24 | (uint32_t)p[i * 4 + 1] << 16 |
               (uint32_t)p[i * 4 + 2] << 8 | p[i * 4 + 3];
    }
    for (int i = 16; i < 64; i++) {
        uint32_t s0 = ROR(w[i - 15], 7) ^ ROR(w[i - 15], 18) ^ (w[i - 15] >> 3);
        uint32_t s1 = ROR(w[i - 2], 17) ^ ROR(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    uint32_t a = s->h[0], b = s->h[1], c = s->h[2], d = s->h[3];
    uint32_t e = s->h[4], f = s->h[5], g = s->h[6], h = s->h[7];
    for (int i = 0; i < 64; i++) {
        uint32_t t1 = h + (ROR(e, 6) ^ ROR(e, 11) ^ ROR(e, 25)) + ((e & f) ^ (~e & g)) + K[i] + w[i];
        uint32_t t2 = (ROR(a, 2) ^ ROR(a, 13) ^ ROR(a, 22)) + ((a & b) ^ (a & c) ^ (b & c));
        h = g; g = f; f = e; e = d + t1; d = c; c = b; b = a; a = t1 + t2;
    }
    s->h[0] += a; s->h[1] += b; s->h[2] += c; s->h[3] += d;
    s->h[4] += e; s->h[5] += f; s->h[6] += g; s->h[7] += h;
}
static void sha_init(struct sha *s) {
    static const uint32_t init[8] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                                     0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};
    memcpy(s->h, init, sizeof(init));
    s->bytes = 0;
}
static void sha_update(struct sha *s, const uint8_t *p, size_t n) {
    while (n != 0) {
        size_t fill = (size_t)(s->bytes % 64);
        size_t take = 64 - fill < n ? 64 - fill : n;
        memcpy(s->block + fill, p, take);
        s->bytes += take; p += take; n -= take;
        if ((s->bytes % 64) == 0) {
            sha_block(s, s->block);
        }
    }
}
static void sha_hex(struct sha *s, char out[65]) {
    uint64_t bits = s->bytes * 8;
    uint8_t pad = 0x80;
    sha_update(s, &pad, 1);
    uint8_t zero = 0;
    while (s->bytes % 64 != 56) {
        sha_update(s, &zero, 1);
    }
    uint8_t len[8];
    for (int i = 0; i < 8; i++) {
        len[i] = (uint8_t)(bits >> (56 - 8 * i));
    }
    sha_update(s, len, 8);
    for (int i = 0; i < 8; i++) {
        snprintf(out + i * 8, 9, "%08x", s->h[i]);
    }
}

static void digest(const uint8_t *p, size_t n, char out[65]) {
    struct sha s;
    sha_init(&s);
    sha_update(&s, p, n);
    sha_hex(&s, out);
}

// ---- helpers ---------------------------------------------------------------
#define RECLAIM_SUFFIX "mode=reclaim\n"
#define RECLAIM_DEADLINE_MS 90000UL

#define MMAPCOST_HEAD "mode=mmapcost\n"
#define CONTRACT_CAPACITY 4096
static char contract_text[CONTRACT_CAPACITY];

// 0: no contract, 1: mapping proof, 2: reclaim proof (contract plus mode line),
// 3: mmap-cost measurement (contract, `mode=mmapcost`, then `file=` rows left
// in `contract_text` for `mmap_cost_probe`).
static int contract_mode(void) {
    int fd = open(CONTRACT_PATH, O_RDONLY);
    if (fd < 0) {
        return 0;
    }
    ssize_t got = read(fd, contract_text, sizeof(contract_text) - 1);
    close(fd);
    size_t head = sizeof(CONTRACT_BODY) - 1;
    if (got < (ssize_t)head || memcmp(contract_text, CONTRACT_BODY, head) != 0) {
        return 0;
    }
    contract_text[got] = 0;
    if (got == (ssize_t)head) {
        return 1;
    }
    size_t tail = sizeof(RECLAIM_SUFFIX) - 1;
    if (got == (ssize_t)(head + tail) && memcmp(contract_text + head, RECLAIM_SUFFIX, tail) == 0) {
        return 2;
    }
    size_t mhead = sizeof(MMAPCOST_HEAD) - 1;
    if (got > (ssize_t)(head + mhead) && memcmp(contract_text + head, MMAPCOST_HEAD, mhead) == 0) {
        return 3;
    }
    return 0;
}

// Finds LIBRARY_PATH's byte length and digest in pagerd's manifest.
static int manifest_row(uint64_t *len, char hash[65]) {
    static char text[2048];
    int fd = open(MANIFEST_PATH, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    ssize_t got = read(fd, text, sizeof(text) - 1);
    close(fd);
    if (got <= 0) {
        return -1;
    }
    text[got] = 0;
    for (char *line = text; line && *line;) {
        char *next = strchr(line, '\n');
        if (next) {
            *next++ = 0;
        }
        char *tab1 = strchr(line, '\t');
        char *tab2 = tab1 ? strchr(tab1 + 1, '\t') : NULL;
        if (tab2 && strcmp(tab2 + 1, LIBRARY_PATH) == 0 && tab2 - tab1 - 1 == 64) {
            *len = strtoull(line, NULL, 10);
            memcpy(hash, tab1 + 1, 64);
            hash[64] = 0;
            return 0;
        }
        line = next;
    }
    return -1;
}

static int same_frames(const struct frame *a, const struct frame *b, unsigned pages) {
    for (unsigned i = 0; i < pages; i++) {
        if (!(a[i].flags & 1) || a[i].phys != b[i].phys) {
            return 0;
        }
    }
    return 1;
}

static int all_read_only(const struct frame *a, unsigned pages) {
    for (unsigned i = 0; i < pages; i++) {
        if (!(a[i].flags & 1) || (a[i].flags & 2)) {
            return 0;
        }
    }
    return 1;
}

static int read_full(int fd, void *buf, size_t n) {
    uint8_t *p = buf;
    while (n) {
        ssize_t got = read(fd, p, n);
        if (got <= 0) {
            return -1;
        }
        p += got; n -= (size_t)got;
    }
    return 0;
}

static int write_full(int fd, const void *buf, size_t n) {
    const uint8_t *p = buf;
    while (n) {
        ssize_t put = write(fd, p, n);
        if (put <= 0) {
            return -1;
        }
        p += put; n -= (size_t)put;
    }
    return 0;
}

#define CHILD_SLEEP_MS 3000L
#define PARENT_LOOK_MS 1500L

static pid_t wait_for(pid_t pid, int *status) {
    pid_t r;
    do {
        r = waitpid(pid, status, 0);
    } while (r < 0 && errno == EINTR);
    return r;
}

static uint8_t reference[MAX_PAGES * PAGE];

static uint64_t map_len_of(uint64_t len) { return (len + PAGE - 1) & ~(PAGE - 1); }

// Maps the library MAP_PRIVATE read/write through a fresh open description at
// `fixed` (or anywhere when zero).
static uint8_t *map_library(uint64_t len, int *fd_out, uint64_t fixed) {
    int fd = open(LIBRARY_PATH, O_RDONLY);
    if (fd < 0) {
        return NULL;
    }
    void *p = mmap((void *)fixed, map_len_of(len), PROT_READ | PROT_WRITE,
                   MAP_PRIVATE, fd, 0);
    *fd_out = fd;
    return p == MAP_FAILED ? NULL : p;
}

// Child exit codes name the first failed check; 0 is success.
static int child_main(uint8_t *inherited, uint64_t len, const char *want_hash) {
    unsigned pages = (unsigned)((len + PAGE - 1) / PAGE);
    int fd = -1;
    uint8_t *fresh = map_library(len, &fd, 0);
    char got[65];
    if (!fresh) return 2;
    digest(fresh, len, got);
    if (strcmp(got, want_hash) != 0) return 3;
    digest(inherited, len, got);
    if (strcmp(got, want_hash) != 0) return 4;
    struct frame f0[MAX_PAGES], i0[MAX_PAGES];
    if (frames_of(fresh, pages, f0) != 0 || frames_of(inherited, pages, i0) != 0) return 5;
    for (unsigned i = 0; i < pages; i++) {
        if (f0[i].phys != i0[i].phys) return 17;
    }

    // The parent inspects this process's frames and writes its own page 3.
    sleep_ms(CHILD_SLEEP_MS);

    struct frame f3, i3;
    digest(fresh, len, got);
    if (strcmp(got, want_hash) != 0) return 6;
    digest(inherited, len, got);
    if (strcmp(got, want_hash) != 0) return 7;
    if (frames_of(fresh + 3 * PAGE, 1, &f3) != 0 || frames_of(inherited + 3 * PAGE, 1, &i3) != 0) return 8;
    if (f3.phys != f0[3].phys || i3.phys != i0[3].phys) return 9;

    // Child write copies privately.
    inherited[5 * PAGE + 7] ^= 0x5a;
    struct frame own;
    if (frames_of(inherited + 5 * PAGE, 1, &own) != 0 || !(own.flags & 2) || own.phys == i0[5].phys) return 10;

    // Fork semantics: unmap two middle pages (splits the alias VMA); a
    // grandchild must still read every surviving page.
    if (munmap(inherited + 10 * PAGE, 2 * PAGE) != 0) return 11;
    pid_t grandchild = fork();
    if (grandchild < 0) return 12;
    if (grandchild == 0) {
        for (unsigned page = 0; page < pages; page++) {
            if (page == 10 || page == 11) continue;
            const uint8_t *seen = inherited + page * PAGE;
            size_t bytes = (page + 1 == pages) ? (size_t)(len - page * PAGE) : PAGE;
            if (page == 5) {
                if (seen[7] == reference[page * PAGE + 7]) _exit(13);
                if (memcmp(seen, reference + page * PAGE, 7) != 0 ||
                    memcmp(seen + 8, reference + page * PAGE + 8, bytes - 8) != 0) _exit(14);
            } else if (memcmp(seen, reference + page * PAGE, bytes) != 0) {
                _exit(15);
            }
        }
        _exit(0);
    }
    int gs = 0;
    if (wait_for(grandchild, &gs) != grandchild || !WIFEXITED(gs) || WEXITSTATUS(gs) != 0) return 16;
    munmap(fresh, map_len_of(len));
    close(fd);
    return 0;
}

// Memory-pressure proof: hold a live alias mapping while pagerd retires cold
// objects; the mapping (pinned frames) must keep serving the same bytes.
static int reclaim_proof(uint64_t len, const char *want, unsigned pages, const struct stats *before) {
    int fd = -1;
    uint8_t *map = map_library(len, &fd, 0);
    if (!map) {
        return fail("reclaim mmap", 0, 0);
    }
    struct stats now;
    if (stats_of(&now) != 0 || now.alias_pages < before->alias_pages + pages) {
        return fail("reclaim mapping was not aliased", 0, 0);
    }
    struct frame held[MAX_PAGES], cur[MAX_PAGES];
    if (frames_of(map, pages, held) != 0) {
        return fail("reclaim frames", 0, 0);
    }
    uint64_t started = now_ms();
    unsigned checks = 0;
    uint64_t sealed_start = now.sealed_objects;
    for (;;) {
        char got[65];
        digest(map, len, got);
        if (strcmp(got, want) != 0) {
            return fail("mapping bytes changed under reclaim", (long)checks, 0);
        }
        if (frames_of(map, pages, cur) != 0 || !same_frames(held, cur, pages)) {
            return fail("mapping frames moved under reclaim", (long)checks, 0);
        }
        checks++;
        if (stats_of(&now) != 0) {
            return fail("stats", 0, 0);
        }
        if (now.sealed_objects < sealed_start) {
            break;
        }
        if (now_ms() - started > RECLAIM_DEADLINE_MS) {
            return fail("no library object was reclaimed under pressure", (long)sealed_start,
                        (long)now.sealed_objects);
        }
        sleep_ms(500);
    }
    // One more full verification after the retire, then a private write.
    char got[65];
    digest(map, len, got);
    if (strcmp(got, want) != 0 || frames_of(map, pages, cur) != 0 || !same_frames(held, cur, pages)) {
        return fail("mapping broken after reclaim", 0, 0);
    }
    map[2 * PAGE + 3] ^= 0x33;
    say("library-map-probe: reclaim-survived sealed=%llu->%llu checks=%u pages=%u",
        (unsigned long long)sealed_start, (unsigned long long)now.sealed_objects, checks, pages);
    munmap(map, map_len_of(len));
    close(fd);
    return 0;
}

// ---- mmap cost measurement (mode 3) ----------------------------------------
// Measures what the CURRENT path costs for a large private file mapping that is
// not in the library cache (docs/ai/pager/library-cache.md, "Measured baseline:
// large non-warmed file mappings"). Read-only observation: it only maps, reads,
// forks and unmaps, and reads the same free-page diagnostic as the other modes.
#define MMAPCOST_MAX_PAGES (1UL << 18)
static uint64_t first_words[MMAPCOST_MAX_PAGES];

static uint64_t ns_now(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000000000UL + (uint64_t)now.tv_nsec;
}

// Same generator as tools/xtask/src/kvm/mmap_cost_probe.rs (splitmix64 of the
// word index), so any word of the file can be re-derived without a second copy.
static uint64_t expected_word(uint64_t seed, uint64_t index) {
    uint64_t z = seed + (index + 1) * 0x9e3779b97f4a7c15UL;
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9UL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebUL;
    return z ^ (z >> 31);
}

static int page_matches(const uint8_t *map, uint64_t seed, uint64_t page) {
    const uint64_t *words = (const uint64_t *)(map + page * PAGE);
    for (uint64_t w = 0; w < PAGE / 8; w++) {
        if (words[w] != expected_word(seed, page * (PAGE / 8) + w)) {
            return 0;
        }
    }
    return 1;
}

static uint64_t us_of(uint64_t from, uint64_t to) { return (to - from) / 1000UL; }

static int free_of(uint64_t *out) {
    struct stats s;
    if (stats_of(&s) != 0) {
        return -1;
    }
    *out = s.free_pages;
    return 0;
}

static void mmap_cost_failed(uint64_t size, const char *stage, int err) {
    say("mmap-cost-probe: size=%llu FAILED stage=%s errno=%d", (unsigned long long)size, stage, err);
}

// Returns 1 for a hard probe failure (wrong data, diagnostic refused); a size
// the system refuses or cannot map is recorded as FAILED and is not a probe error.
static int mmap_cost_one(const char *path, uint64_t size, uint64_t seed, const char *want_sha) {
    uint64_t pages = size / PAGE;
    if (size % PAGE != 0 || pages == 0 || pages > MMAPCOST_MAX_PAGES) {
        return fail("mmap-cost size shape", (long)size, 0);
    }
    uint64_t free_before, free_mmap, free_sparse, free_full, free_fork = 0, free_unmap;
    struct stats leaves_before, leaves_mapped;
    if (free_of(&free_before) != 0 || stats_of(&leaves_before) != 0) {
        return fail("stats", 0, 0);
    }
    say("mmap-cost-probe: begin size=%llu path=%s", (unsigned long long)size, path);
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        mmap_cost_failed(size, "open", errno);
        return 0;
    }
    uint64_t t0 = ns_now();
    void *raw = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    uint64_t t1 = ns_now();
    if (raw == MAP_FAILED) {
        mmap_cost_failed(size, "mmap", errno);
        close(fd);
        return 0;
    }
    const uint8_t *map = raw;
    uint64_t mmap_us = us_of(t0, t1);
    if (free_of(&free_mmap) != 0 || stats_of(&leaves_mapped) != 0) {
        return fail("stats", 0, 0);
    }

    // (b) sparse: one 8-byte read from each of ~1% of the pages, spread evenly.
    uint64_t sparse_n = pages / 100 ? pages / 100 : 1;
    uint64_t sparse_pages[MMAPCOST_MAX_PAGES / 100 + 1];
    uint64_t sparse_got[MMAPCOST_MAX_PAGES / 100 + 1];
    for (uint64_t k = 0; k < sparse_n; k++) {
        sparse_pages[k] = k * pages / sparse_n;
    }
    t0 = ns_now();
    for (uint64_t k = 0; k < sparse_n; k++) {
        sparse_got[k] = *(const volatile uint64_t *)(map + sparse_pages[k] * PAGE);
    }
    t1 = ns_now();
    uint64_t sparse_us = us_of(t0, t1);
    for (uint64_t k = 0; k < sparse_n; k++) {
        // Untimed: the touched pages must hold exactly the expected content.
        if (sparse_got[k] != expected_word(seed, sparse_pages[k] * (PAGE / 8)) ||
            !page_matches(map, seed, sparse_pages[k])) {
            return fail("sparse page content", (long)size, (long)sparse_pages[k]);
        }
    }
    if (free_of(&free_sparse) != 0) {
        return fail("stats", 0, 0);
    }

    // (c) every page: one 8-byte read per page, recorded and checked afterwards.
    t0 = ns_now();
    for (uint64_t page = 0; page < pages; page++) {
        first_words[page] = *(const volatile uint64_t *)(map + page * PAGE);
    }
    t1 = ns_now();
    uint64_t full_us = us_of(t0, t1);
    for (uint64_t page = 0; page < pages; page++) {
        if (first_words[page] != expected_word(seed, page * (PAGE / 8))) {
            return fail("full-touch page content", (long)size, (long)page);
        }
    }
    if (free_of(&free_full) != 0) {
        return fail("stats", 0, 0);
    }

    // (d) fork with the mapping live; the child reads four spread pages, then
    // waits so the parent can sample free pages while the child exists.
    int to_child[2], from_child[2];
    if (pipe(to_child) != 0 || pipe(from_child) != 0) {
        return fail("pipe", 0, 0);
    }
    t0 = ns_now();
    pid_t child = fork();
    t1 = ns_now();
    if (child < 0) {
        mmap_cost_failed(size, "fork", errno);
    } else if (child == 0) {
        int code = 0;
        uint64_t picks[4] = {0, pages / 3, (2 * pages) / 3, pages - 1};
        for (int i = 0; i < 4; i++) {
            if (!page_matches(map, seed, picks[i])) {
                code = 30 + i;
            }
        }
        char ready = (char)code;
        (void)write(from_child[1], &ready, 1);
        char sink;
        (void)read(to_child[0], &sink, 1);
        _exit(code);
    }
    uint64_t fork_us = us_of(t0, t1);
    int child_status = 0;
    int child_ok = 0;
    if (child > 0) {
        char ready = 0x7f;
        if (read(from_child[0], &ready, 1) == 1 && ready == 0) {
            child_ok = 1;
        }
        if (free_of(&free_fork) != 0) {
            return fail("stats", 0, 0);
        }
        close(to_child[1]);
        if (wait_for(child, &child_status) != child || !WIFEXITED(child_status) ||
            WEXITSTATUS(child_status) != 0) {
            child_ok = 0;
        }
    }
    close(to_child[0]); close(from_child[0]); close(from_child[1]);
    if (child > 0 && !child_ok) {
        return fail("fork child content", (long)size, child_status);
    }

    // Whole-file integrity against the host's sha256 (untimed for the table).
    t0 = ns_now();
    char got[65];
    digest(map, size, got);
    t1 = ns_now();
    uint64_t sha_us = us_of(t0, t1);
    if (strcmp(got, want_sha) != 0) {
        return fail("file sha256 differs from the host digest", (long)size, 0);
    }

    t0 = ns_now();
    int unmapped = munmap(raw, size);
    t1 = ns_now();
    uint64_t unmap_us = us_of(t0, t1);
    if (unmapped != 0) {
        return fail("munmap", (long)size, 0);
    }
    close(fd);
    sleep_ms(500);
    if (free_of(&free_unmap) != 0) {
        return fail("stats", 0, 0);
    }
    say("mmap-cost-probe: size=%llu mmap_us=%llu sparse_us=%llu sparse_pages=%llu full_us=%llu "
        "fork_us=%lld unmap_us=%llu sha_us=%llu free_before=%llu free_after_mmap=%llu "
        "free_after_sparse=%llu free_after_full=%llu free_fork_live=%llu free_after_unmap=%llu "
        "leaves_mmap=%lld sha_ok=1 verify_ok=1",
        (unsigned long long)size, (unsigned long long)mmap_us, (unsigned long long)sparse_us,
        (unsigned long long)sparse_n, (unsigned long long)full_us,
        child > 0 ? (long long)fork_us : -1LL, (unsigned long long)unmap_us,
        (unsigned long long)sha_us, (unsigned long long)free_before, (unsigned long long)free_mmap,
        (unsigned long long)free_sparse, (unsigned long long)free_full,
        (unsigned long long)free_fork, (unsigned long long)free_unmap,
        (long long)leaves_mapped.own_data_leaves - (long long)leaves_before.own_data_leaves);
    return 0;
}

static int mmap_cost_probe(void) {
    unsigned rows = 0;
    for (char *line = contract_text; line && *line;) {
        char *next = strchr(line, '\n');
        if (next) {
            *next++ = 0;
        }
        char path[256], sha[65];
        unsigned long long size = 0, seed = 0;
        if (strncmp(line, "file=", 5) == 0) {
            if (sscanf(line + 5, "%255s %llu %llx %64s", path, &size, &seed, sha) != 4) {
                return fail("mmap-cost contract row", (long)rows, 0);
            }
            int bad = mmap_cost_one(path, size, seed, sha);
            if (bad) {
                return bad;
            }
            rows++;
        }
        line = next;
    }
    if (rows == 0) {
        return fail("mmap-cost contract has no files", 0, 0);
    }
    say("mmap-cost-probe: done files=%u", rows);
    return 0;
}

int main(void) {
    int mode = contract_mode();
    if (mode == 0) {
        return 0;
    }
    // The reclaim proof must hold its mapping before the hog (3 s delay) drains memory.
    if (mode == 1 || mode == 3) {
        sleep_ms((long)START_DELAY_MS);
    }

    struct stats s0;
    uint64_t waited = now_ms();
    for (;;) {
        if (stats_of(&s0) != 0) {
            return fail("diagnostic refused", 0, 0);
        }
        if (s0.sealed_objects >= WARM_ROWS) {
            break;
        }
        if (now_ms() - waited > WARM_DEADLINE_MS) {
            say("library-map-probe: warm incomplete sealed=%llu of %u",
                (unsigned long long)s0.sealed_objects, WARM_ROWS);
            break;
        }
        sleep_ms(200);
    }

    if (mode == 3) {
        // The warm worker's reads must not overlap the measurement.
        sleep_ms(1500);
        return mmap_cost_probe();
    }

    uint64_t len = 0;
    char want[65];
    if (manifest_row(&len, want) != 0) {
        return fail("manifest row missing", 0, 0);
    }
    unsigned pages = (unsigned)((len + PAGE - 1) / PAGE);
    if (pages < 12 || pages > MAX_PAGES || len % PAGE == 0) {
        return fail("library shape", (long)len, pages);
    }

    if (mode == 2) {
        struct stats first;
        if (stats_of(&first) != 0) {
            return fail("stats", 0, 0);
        }
        return reclaim_proof(len, want, pages, &first);
    }

    // Oracle: the bytes by ordinary read(), independent of the cache.
    int rfd = open(LIBRARY_PATH, O_RDONLY);
    if (rfd < 0 || read_full(rfd, reference, len) != 0) {
        return fail("reference read", 0, 0);
    }
    close(rfd);
    char got[65];
    digest(reference, len, got);
    if (strcmp(got, want) != 0) {
        return fail("reference digest differs from manifest", 0, 0);
    }

    struct stats before, mapped, after_cow, after_unmap, final;
    if (stats_of(&before) != 0) {
        return fail("stats", 0, 0);
    }
    int fd = -1;
    uint8_t *map = map_library(len, &fd, 0);
    if (!map) {
        return fail("mmap", 0, 0);
    }
    if (stats_of(&mapped) != 0) {
        return fail("stats", 0, 0);
    }
    if (mapped.alias_maps != before.alias_maps + 1 ||
        mapped.alias_pages != before.alias_pages + pages) {
        // decline reasons: 0 nothing sealed, 1 attest failed (errno), 2 no
        // matching object, 3 unbound, 4 out of range, 5 pin failed, 6 admission.
        for (int i = 0; i < 7; i++) {
            say("library-map-probe: decline[%d] count=%llu->%llu detail=%llu", i,
                (unsigned long long)before.decline_counts[i],
                (unsigned long long)mapped.decline_counts[i],
                (unsigned long long)mapped.decline_details[i]);
        }
        return fail("mapping was not served by aliasing", (long)(mapped.alias_maps - before.alias_maps),
                    (long)(mapped.alias_pages - before.alias_pages));
    }
    if (mapped.own_data_leaves < before.own_data_leaves + pages) {
        return fail("alias leaves missing", (long)mapped.own_data_leaves, (long)before.own_data_leaves);
    }
    digest(map, len, got);
    if (strcmp(got, want) != 0 || memcmp(map, reference, len) != 0) {
        return fail("mapped bytes differ from file hash", 0, 0);
    }
    for (uint64_t i = len; i < (uint64_t)pages * PAGE; i++) {
        if (map[i] != 0) {
            return fail("EOF tail not zero", (long)i, map[i]);
        }
    }
    struct frame parent[MAX_PAGES];
    if (frames_of(map, pages, parent) != 0 || !all_read_only(parent, pages)) {
        return fail("parent frames", 0, 0);
    }
    say("library-map-probe: mapped pages=%u alias_maps=%llu leaves+%llu free-%lld", pages,
        (unsigned long long)mapped.alias_maps,
        (unsigned long long)(mapped.own_data_leaves - before.own_data_leaves),
        (long long)before.free_pages - (long long)mapped.free_pages);

    // A second, independent open+mmap in the same process; the child inherits it.
    int fd2 = -1;
    uint8_t *map2 = map_library(len, &fd2, 0);
    if (!map2) {
        return fail("second mmap", 0, 0);
    }
    struct frame second[MAX_PAGES];
    if (frames_of(map2, pages, second) != 0 || !same_frames(parent, second, pages)) {
        return fail("second mapping does not share frames", 0, 0);
    }
    if (stats_of(&mapped) != 0) {
        return fail("stats", 0, 0);
    }
    pid_t child = fork();
    if (child < 0) {
        return fail("fork", 0, 0);
    }
    if (child == 0) {
        _exit(child_main(map, len, want));
    }
    sleep_ms(PARENT_LOOK_MS);

    // Second process: its fresh mapping and its fork-inherited mapping sit on
    // the parent's frames.
    struct frame cf[MAX_PAGES], ci[MAX_PAGES];
    if (frames_of_pid((uint64_t)child, map2, pages, cf) != 0 ||
        frames_of_pid((uint64_t)child, map, pages, ci) != 0) {
        int early = 0;
        pid_t reaped = waitpid(child, &early, WNOHANG);
        say("library-map-probe: child pid=%d reaped=%d exited=%d code=%d", (int)child, (int)reaped,
            WIFEXITED(early), WEXITSTATUS(early));
        return fail("child frames", 0, 0);
    }
    if (!same_frames(parent, cf, pages)) {
        return fail("second process does not share frames", 0, 0);
    }
    if (!same_frames(parent, ci, pages)) {
        return fail("fork-inherited mapping does not share frames", 0, 0);
    }
    if (!all_read_only(cf, pages) || !all_read_only(ci, pages)) {
        return fail("child leaves are writable", 0, 0);
    }
    say("library-map-probe: shared frames=%u processes=2 fork-inherited=%u", pages, pages);

    // Private write: parent copies page 3 only; the child stays on the original.
    uint8_t original = map[3 * PAGE + 11];
    map[3 * PAGE + 11] = (uint8_t)(original ^ 0xa5);
    struct frame cow[MAX_PAGES];
    if (frames_of(map, pages, cow) != 0) {
        return fail("frames after write", 0, 0);
    }
    if (!(cow[3].flags & 2) || cow[3].phys == parent[3].phys) {
        return fail("write did not copy the page", (long)cow[3].phys, (long)parent[3].phys);
    }
    for (unsigned i = 0; i < pages; i++) {
        if (i != 3 && cow[i].phys != parent[i].phys) {
            return fail("write moved another page", i, 0);
        }
    }
    struct frame cf3, ci3;
    if (frames_of_pid((uint64_t)child, map2 + 3 * PAGE, 1, &cf3) != 0 ||
        frames_of_pid((uint64_t)child, map + 3 * PAGE, 1, &ci3) != 0 ||
        cf3.phys != parent[3].phys || ci3.phys != parent[3].phys) {
        return fail("child frame moved by parent write", 0, 0);
    }
    if (stats_of(&after_cow) != 0 ||
        labs((long)after_cow.own_data_leaves - (long)mapped.own_data_leaves) > 2) {
        return fail("leaf count changed by COW", (long)after_cow.own_data_leaves,
                    (long)mapped.own_data_leaves);
    }
    say("library-map-probe: cow-isolated parent-write child-frame-unchanged");

    int status = 0;
    if (wait_for(child, &status) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return fail("child checks (exit status)", WIFEXITED(status) ? WEXITSTATUS(status) : -1, status);
    }
    // The child's own write (page 5) and split never reached this process.
    struct frame mine;
    if (frames_of(map + 5 * PAGE, 1, &mine) != 0 || mine.phys != parent[5].phys ||
        map[5 * PAGE + 7] != reference[5 * PAGE + 7]) {
        return fail("child write leaked into parent", 0, 0);
    }
    say("library-map-probe: eof-zero tail=%llu", (unsigned long long)(pages * PAGE - len));

    // Fork after a private write: a second child inherits the mixed mapping.
    pid_t late = fork();
    if (late < 0) {
        return fail("late fork", 0, 0);
    }
    if (late == 0) {
        struct frame mixed[MAX_PAGES];
        if (frames_of(map, pages, mixed) != 0) _exit(21);
        if (map[3 * PAGE + 11] != (uint8_t)(original ^ 0xa5)) _exit(22);
        // Page 3 is anonymous after the private write: fork may share or copy it.
        for (unsigned i = 0; i < pages; i++) {
            if (i != 3 && mixed[i].phys != parent[i].phys) _exit(24);
            size_t bytes = (i + 1 == pages) ? (size_t)(len - i * PAGE) : PAGE;
            if (i != 3 && memcmp(map + i * PAGE, reference + i * PAGE, bytes) != 0) _exit(25);
        }
        _exit(0);
    }
    int late_status = 0;
    if (wait_for(late, &late_status) != late || !WIFEXITED(late_status) || WEXITSTATUS(late_status) != 0) {
        return fail("fork after private write", late_status, 0);
    }
    say("library-map-probe: fork-semantics split-grandchild ok post-write-fork ok");

    sleep_ms(300);
    if (munmap(map, map_len_of(len)) != 0) {
        return fail("munmap", 0, 0);
    }
    close(fd);
    if (munmap(map2, map_len_of(len)) != 0) {
        return fail("munmap second", 0, 0);
    }
    close(fd2);
    sleep_ms(300);
    if (stats_of(&after_unmap) != 0) {
        return fail("stats", 0, 0);
    }
    if (labs((long)after_unmap.own_data_leaves - (long)before.own_data_leaves) > LEAF_TOLERANCE) {
        return fail("leaves not released", (long)after_unmap.own_data_leaves, (long)before.own_data_leaves);
    }
    if (stats_of(&final) != 0) {
        return fail("stats", 0, 0);
    }
    long drift = (long)final.free_pages - (long)before.free_pages;
    if (drift < -FREE_TOLERANCE_PAGES) {
        return fail("frames not returned drift", drift, 0);
    }
    if (final.sealed_objects != before.sealed_objects) {
        return fail("registry changed", 0, 0);
    }
    say("library-map-probe: shared frames=%u cow-isolated eof-zero exit-clean leaves=%llu free-drift=%ld",
        pages, (unsigned long long)final.own_data_leaves, drift);
    return 0;
}
