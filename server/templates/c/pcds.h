/* pcds.h: the plugin ABI of the Planet Crafter dedicated server (docs/plugins-api.md), for C.
 *
 * No libc, no WASI: only what this file declares. Build with
 *   clang --target=wasm32 -nostdlib -O2 -Wl,--no-entry -o plugin.wasm plugin.c
 * (the Makefile does). A plugin defines  void on_command(...)  and the like; this file exports
 * everything the server needs and turns the bytes of an event into plain values.
 * MIT OR Apache-2.0, like the Rust crate: copy it into your plugin.
 */
#ifndef PCDS_H
#define PCDS_H

typedef unsigned char u8;
typedef unsigned int u32;
typedef int i32;
typedef unsigned long long u64;
typedef long long i64;

#define PCDS_ABI ((1u << 16) | 3u)

/* ---- what the server provides (module "pcds"). Each returns 0, or a negative code: -1 denied
 * (capability not granted), -2 bad arguments, -3 queue full, -4 unsupported, -5 not allowed, -6 not found. */
#define PCDS_IMPORT(name) __attribute__((import_module("pcds"), import_name(name)))
PCDS_IMPORT("log") void pcds_log(i32 level, const char *text, u32 len);
PCDS_IMPORT("say") i32 pcds_say(const char *text, u32 len);
PCDS_IMPORT("tell") i32 pcds_tell(u64 client, const char *text, u32 len);
PCDS_IMPORT("kick") i32 pcds_kick(u64 client, const char *text, u32 len);
PCDS_IMPORT("query") i32 pcds_query(u32 kind, const void *arg, u32 arg_len, void *out, u32 cap);

/* ---- a bump allocator over a fixed arena: the server asks for a buffer for each event and
 * gives it back after, so the arena is used last in, first out. */
#ifndef PCDS_ARENA
#define PCDS_ARENA (256 * 1024)
#endif
static u8 pcds_arena[PCDS_ARENA] __attribute__((aligned(16)));
static u32 pcds_top;

__attribute__((export_name("pcds_abi"))) u32 pcds_abi(void) { return PCDS_ABI; }

__attribute__((export_name("pcds_alloc"))) u32 pcds_alloc(u32 len) {
    u32 at = (pcds_top + 15u) & ~15u;
    if (len > PCDS_ARENA || at > PCDS_ARENA - len) return 0;
    pcds_top = at + len;
    return (u32)(unsigned long)&pcds_arena[at];
}

__attribute__((export_name("pcds_free"))) void pcds_free(u32 ptr, u32 len) {
    u32 at = ptr - (u32)(unsigned long)&pcds_arena[0];
    if (at + len <= pcds_top && ((at + len + 15u) & ~15u) >= pcds_top) pcds_top = at;
}

/* ---- reading an event: little-endian numbers, strings as a u32 length and UTF-8 bytes. */
typedef struct { const u8 *p; u32 left; int bad; } pcds_reader;
typedef struct { const char *p; u32 len; } pcds_str;

static void pcds_take(pcds_reader *r, void *out, u32 n) {
    if (r->left < n) { r->bad = 1; r->left = 0; for (u32 i = 0; i < n; i++) ((u8 *)out)[i] = 0; return; }
    for (u32 i = 0; i < n; i++) ((u8 *)out)[i] = r->p[i];
    r->p += n; r->left -= n;
}
static u32 pcds_u32(pcds_reader *r) { u32 v; pcds_take(r, &v, 4); return v; }
static u64 pcds_u64(pcds_reader *r) { u64 v; pcds_take(r, &v, 8); return v; }
static pcds_str pcds_string(pcds_reader *r) {
    u32 n = pcds_u32(r); pcds_str s = {0, 0};
    if (r->bad || r->left < n) { r->bad = 1; return s; }
    s.p = (const char *)r->p; s.len = n; r->p += n; r->left -= n; return s;
}

static int pcds_str_is(pcds_str s, const char *lit) {
    u32 i = 0;
    for (; i < s.len; i++) if (lit[i] == 0 || lit[i] != s.p[i]) return 0;
    return lit[i] == 0;
}

/* ---- writing a text (no libc: a few helpers). */
typedef struct { char *p; u32 len; u32 cap; } pcds_text;
static void pcds_put(pcds_text *t, const char *s, u32 n) { for (u32 i = 0; i < n && t->len < t->cap; i++) t->p[t->len++] = s[i]; }
static void pcds_puts(pcds_text *t, const char *s) { while (*s) pcds_put(t, s++, 1); }
static void pcds_putu(pcds_text *t, u64 v) {
    char d[20]; u32 n = 0;
    do { d[n++] = (char)('0' + v % 10); v /= 10; } while (v);
    while (n) pcds_put(t, &d[--n], 1);
}

/* ---- the event kinds (docs/plugins-api.md). */
enum { PCDS_TICK = 1, PCDS_JOIN = 2, PCDS_LEAVE = 3, PCDS_CHAT = 4, PCDS_COMMAND = 5, PCDS_SAVED = 10 };

/* What your plugin implements (see plugin.c). */
static void on_load(const u8 *state, u32 len, u32 version);
static void on_command(u64 client, pcds_str name, pcds_str command, pcds_reader *args, u32 argc);
static u32 on_save(u8 *out, u32 cap, u32 *version);

static u32 pcds_saved_version;

__attribute__((export_name("pcds_event"))) i32 pcds_event(u32 kind, const u8 *payload, u32 len) {
    pcds_reader r = {payload, len, 0};
    if (kind == PCDS_COMMAND) {
        u64 client = pcds_u64(&r);
        pcds_str name = pcds_string(&r);
        pcds_str command = pcds_string(&r);
        u32 argc = pcds_u32(&r);
        if (r.bad) return -1;
        on_command(client, name, command, &r, argc);
    }
    return 0;
}

__attribute__((export_name("pcds_load"))) i32 pcds_load(const u8 *state, u32 len, u32 version) {
    on_load(state, len, version);
    return 0;
}

/* The state: the bytes, as (pointer << 32) | length; the version is asked for next. */
__attribute__((export_name("pcds_save"))) u64 pcds_save(void) {
    u32 cap = 256, ptr = pcds_alloc(cap);
    if (!ptr) return 0;
    u32 n = on_save((u8 *)(unsigned long)ptr, cap, &pcds_saved_version);
    return n ? (((u64)ptr << 32) | n) : 0;
}

__attribute__((export_name("pcds_state_version"))) u32 pcds_state_version(void) { return pcds_saved_version; }

#endif
