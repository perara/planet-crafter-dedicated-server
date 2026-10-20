/* pcds.h: the plugin ABI 1.3 of the Planet Crafter dedicated server (docs/plugins-api.md), for C.
 *
 * No libc, no WASI: only what this file declares. Build with
 *   clang --target=wasm32 -nostdlib -O2 -Wall -Wextra -Wl,--no-entry -o plugin.wasm plugin.c
 * Copy this file into your plugin. MIT OR Apache-2.0, like the Rust crate.
 *
 * How to use it: include it once, from your one .c file, and define three functions
 *
 *   static void on_load(const u8 *state, u32 len, u32 version);   // once, at the start
 *   static i32  on_event(const pcds_ev *ev);                   // for each event; 0 or 1 (veto)
 *   static u32  on_save(u8 *out, u32 cap, u32 *version);          // the state to keep (0 bytes: none)
 *
 * The header does the rest: it exports what the server needs (memory, the allocator, the events,
 * the state), reads the bytes of every event into one plain struct, and keeps a bad payload from
 * ever being read past its end.
 *
 * Numbers are little-endian (WebAssembly is). A string is a pointer and a length, UTF-8, not
 * zero-ended, and it points into the event's buffer: copy what you keep past the call.
 */
#ifndef PCDS_H
#define PCDS_H

typedef unsigned char u8;
typedef unsigned int u32;
typedef int i32;
typedef unsigned long long u64;
typedef long long i64;

/* The ABI this header speaks: major << 16 | minor. The server runs a module of the same major and
 * a minor not newer than its own. */
#define PCDS_ABI ((1u << 16) | 3u)

/* ---- return codes of the imports below: 0 is done (the action is queued, the server applies it
 * after your call returns, and it can still fail then; nobody tells you). A negative code means it
 * did not happen. */
enum {
    PCDS_E_DENIED = -1,      /* the capability is not granted */
    PCDS_E_BAD = -2,         /* the arguments are no good */
    PCDS_E_FULL = -3,        /* more than 16 actions in this call, or too many this second */
    PCDS_E_UNSUPPORTED = -4, /* the server is older than the function */
    PCDS_E_NOT_ALLOWED = -5, /* net.http: the host is not on the manifest's list */
    PCDS_E_NOT_FOUND = -6    /* a player or object that is not there */
};

/* ---- what the server provides (module "pcds"). Only the imports you call end up in the module. */
#define PCDS_IMPORT(name) __attribute__((import_module("pcds"), import_name(name)))

/* log: a line in the server's log, prefixed with the plugin's id. level: 0 error, 1 warn, 2 info,
 * 3 debug. Rate limited (20 lines a second); cut at 240 characters. No capability. */
PCDS_IMPORT("log") void pcds_log(i32 level, const char *text, u32 len);
/* say: a chat message from the server to everyone. Capability chat.say. Cut at 500 characters. */
PCDS_IMPORT("say") i32 pcds_say(const char *text, u32 len);
/* tell: a chat message to one player (`client` is the connection number). Capability chat.say. */
PCDS_IMPORT("tell") i32 pcds_tell(u64 client, const char *text, u32 len);
/* kick: removes a player, with a reason. Capability player.kick. */
PCDS_IMPORT("kick") i32 pcds_kick(u64 client, const char *text, u32 len);
/* teleport: moves a player. Capability player.teleport. */
PCDS_IMPORT("teleport") i32 pcds_teleport(u64 client, float x, float y, float z);
/* rules_patch: a JSON merge patch of the server rules, as `pcds rules set` sends it. Capability
 * rules.write. */
PCDS_IMPORT("rules_patch") i32 pcds_rules_patch(const char *json, u32 len);
/* give: puts `count` (at most 50) of the item `group` into a player's backpack. Capability
 * player.give. */
PCDS_IMPORT("give") i32 pcds_give(u64 client, const char *group, u32 len, u32 count);
/* tokens: adds Terra tokens to the world, or takes them away (negative). Capability world.tokens. */
PCDS_IMPORT("tokens") i32 pcds_tokens(i32 amount);
/* timer_set: calls you with a PCDS_TIMER event after delay_ms of game time, then every repeat_ms
 * (0: once). The same id again replaces the timer. At most 16 timers; timers are not kept over a
 * reload or a restart: set them in on_load. Capability event.timer. */
PCDS_IMPORT("timer_set") i32 pcds_timer_set(u32 id, u32 delay_ms, u32 repeat_ms);
/* timer_cancel: cancels a timer (one that is not set is no error). Capability event.timer. */
PCDS_IMPORT("timer_cancel") i32 pcds_timer_cancel(u32 id);
/* query: asks the world (capability world.read for 1..6; 7 needs none). `kind` is a PCDS_Q_*
 * constant, `arg` its argument (at most 64 bytes). The answer is written to `out`, at most `cap`
 * bytes; the return value is the answer's full length (more than `cap` means it was cut: ask again
 * with a bigger buffer), or a negative code. The answers are in the encoding events use. */
PCDS_IMPORT("query") i32 pcds_query(u32 kind, const void *arg, u32 arg_len, void *out, u32 cap);
enum {
    PCDS_Q_WORLD = 1,     /* game_ms i64, planet str, players u32, world_objects u32, network_objects u32,
                           * terraformation f64, standing_still u8, unix_ms i64 */
    PCDS_Q_PLAYERS = 2,   /* count u32, then per player: client u64, name str, steam_id str, position 3 x f32,
                           * gauges 4 x f32, player_id u64, operator u8 */
    PCDS_Q_RULES = 3,     /* the rules that differ from the defaults, as JSON text */
    PCDS_Q_INVENTORY = 4, /* arg: client u64 */
    PCDS_Q_NEARBY = 5,    /* arg: center 3 x f32, radius f32, max u32 */
    PCDS_Q_OBJECT = 6,    /* arg: u8 0 + world id as u64, or u8 1 + network id as u64 */
    PCDS_Q_CONFIG = 7     /* the plugin's own [[config]] settings: count u32, then key str, value str */
};
/* Not wrapped here: http_request (net.http); see docs/plugins-api.md for its encoding. */

/* ---- a bump allocator over a fixed arena: the server asks for a buffer for each event and gives
 * it back after, so the arena is used last in, first out. Make it bigger with -DPCDS_ARENA=... if
 * the events or the state you handle are bigger. */
#ifndef PCDS_ARENA
#define PCDS_ARENA (256 * 1024)
#endif
/* The most bytes on_save may write. */
#ifndef PCDS_SAVE_CAP
#define PCDS_SAVE_CAP 1024
#endif
static u8 pcds_arena[PCDS_ARENA] __attribute__((aligned(16)));
static u32 pcds_top;

#define PCDS_EXPORT(name) __attribute__((export_name(name)))

/* pcds_abi: the ABI the module was built for. */
PCDS_EXPORT("pcds_abi") u32 pcds_abi(void) { return PCDS_ABI; }

/* pcds_alloc: a buffer of `len` bytes for the server to write an event or a state into. 0 if the
 * arena is full (the server then reports a fault). */
PCDS_EXPORT("pcds_alloc") u32 pcds_alloc(u32 len) {
    u32 at = (pcds_top + 15u) & ~15u;
    if (len > PCDS_ARENA || at > PCDS_ARENA - len) return 0;
    pcds_top = at + len;
    return (u32)(unsigned long)&pcds_arena[at];
}

/* pcds_free: gives back the buffer last allocated (the arena is a stack). A pointer that is not
 * the top of the stack, or not in the arena, is ignored. */
PCDS_EXPORT("pcds_free") void pcds_free(u32 ptr, u32 len) {
    u32 base = (u32)(unsigned long)&pcds_arena[0];
    if (ptr < base || ptr - base > pcds_top) return;
    u32 at = ptr - base;
    if (len <= pcds_top - at && ((at + len + 15u) & ~15u) >= pcds_top) pcds_top = at;
}

/* ---- reading bytes: every read checks what is left. A read past the end sets `bad`, returns 0 (or
 * an empty string) and leaves nothing more to read. */
typedef struct { const u8 *p; u32 left; int bad; } pcds_reader;
typedef struct { const char *p; u32 len; } pcds_str;

static inline void pcds_take(pcds_reader *r, void *out, u32 n) {
    if (r->bad || r->left < n) {
        r->bad = 1; r->left = 0;
        for (u32 i = 0; i < n; i++) ((u8 *)out)[i] = 0;
        return;
    }
    for (u32 i = 0; i < n; i++) ((u8 *)out)[i] = r->p[i];
    r->p += n; r->left -= n;
}
static inline u8 pcds_u8(pcds_reader *r) { u8 v; pcds_take(r, &v, 1); return v; }
static inline u32 pcds_u32(pcds_reader *r) { u32 v; pcds_take(r, &v, 4); return v; }
static inline u64 pcds_u64(pcds_reader *r) { u64 v; pcds_take(r, &v, 8); return v; }
static inline i32 pcds_i32(pcds_reader *r) { return (i32)pcds_u32(r); }
static inline i64 pcds_i64(pcds_reader *r) { return (i64)pcds_u64(r); }
static inline float pcds_f32(pcds_reader *r) { union { u32 u; float f; } c; c.u = pcds_u32(r); return c.f; }
/* A string or a byte array: a u32 length and the bytes. Not checked to be UTF-8 (the server only
 * sends UTF-8 in strings, but bytes of a chat line are a player's). */
static inline pcds_str pcds_string(pcds_reader *r) {
    u32 n = pcds_u32(r);
    pcds_str s = {0, 0};
    if (r->bad || r->left < n) { r->bad = 1; r->left = 0; return s; }
    s.p = (const char *)r->p; s.len = n; r->p += n; r->left -= n;
    return s;
}

static inline int pcds_str_is(pcds_str s, const char *lit) {
    u32 i = 0;
    for (; i < s.len; i++) if (lit[i] == 0 || lit[i] != s.p[i]) return 0;
    return lit[i] == 0;
}

/* ---- writing a text: a fixed buffer that never overflows (what does not fit is dropped). */
typedef struct { char *p; u32 len; u32 cap; } pcds_text;
static inline void pcds_put(pcds_text *t, const char *s, u32 n) { for (u32 i = 0; i < n && t->len < t->cap; i++) t->p[t->len++] = s[i]; }
static inline void pcds_puts(pcds_text *t, const char *s) { while (*s) pcds_put(t, s++, 1); }
static inline void pcds_putu(pcds_text *t, u64 v) {
    char d[20]; u32 n = 0;
    do { d[n++] = (char)('0' + v % 10); v /= 10; } while (v);
    while (n) pcds_put(t, &d[--n], 1);
}
static inline void pcds_puti(pcds_text *t, i64 v) {
    if (v < 0) { pcds_puts(t, "-"); pcds_putu(t, (u64)0 - (u64)v); } else pcds_putu(t, (u64)v);
}

/* ---- the events. `kind` is one of these. */
enum {
    PCDS_TICK = 1,             /* capability event.tick; every tick_every_ms of game time */
    PCDS_JOIN = 2,             /* event.player */
    PCDS_LEAVE = 3,            /* event.player */
    PCDS_CHAT = 4,             /* event.chat; every chat line */
    PCDS_COMMAND = 5,          /* event.chat; a chat line that names one of your [[command]]s */
    PCDS_RPC_BEFORE = 6,       /* event.rpc, and rpc.veto to answer 1; a client RPC about to run */
    PCDS_RPC_AFTER = 7,        /* event.rpc */
    PCDS_OBJECT_PLACED = 8,    /* event.object */
    PCDS_OBJECT_DESTROYED = 9, /* event.object */
    PCDS_WORLD_SAVED = 10,     /* event.world */
    PCDS_RULE_CHANGE = 11,     /* event.world */
    PCDS_HTTP_DONE = 12,       /* an answer to your net.http request; always delivered to you */
    PCDS_TIMER = 13,           /* a timer you set is due; always delivered to you */
    PCDS_FLIP = 14             /* event.world; the first event after a hot update of the server */
};

/* One event, read. Only the fields of its kind are set; the others are 0 or empty.
 *
 *   PCDS_TICK            game_ms, tick, players, since_ms, unix_ms
 *   PCDS_JOIN            client, name, steam_id (empty unless verified), player_id
 *   PCDS_LEAVE           client, name, player_id
 *   PCDS_CHAT            client, name, text, player_id
 *   PCDS_COMMAND         client, name, command (without the prefix), text (as typed), player_id,
 *                        argc and pcds_arg(ev, i) for the arguments
 *   PCDS_RPC_BEFORE/AFTER  client, rpc (the name), object (a network object id), args_json
 *   PCDS_OBJECT_PLACED/DESTROYED  id, group, position, client (0: nobody did it)
 *   PCDS_WORLD_SAVED     game_ms, ok
 *   PCDS_RULE_CHANGE     changes_json
 *   PCDS_HTTP_DONE       ticket, status (0: no answer; error says why), error, body (headers are not read)
 *   PCDS_TIMER           timer_id, game_ms, late_ms
 *   PCDS_FLIP            game_ms, server_version
 *
 * `client` is the connection number of this visit; `player_id` is the same for the same player on
 * every visit: key anything you keep by it. */
typedef struct pcds_ev {
    u32 kind;
    u64 client, player_id, object, ticket;
    pcds_str name, text, command, steam_id, rpc, args_json, group, error, body, server_version, changes_json;
    u32 argc;
    pcds_reader args; /* the arguments of a command, positioned at the first */
    i64 game_ms, unix_ms;
    i32 tick, id;
    u32 players, since_ms, status, timer_id, late_ms;
    float position[3];
    int ok;
} pcds_ev;

/* The i-th argument of a command (empty, with p == 0, if there is none). */
static inline pcds_str pcds_arg(const pcds_ev *ev, u32 i) {
    pcds_reader r = ev->args;
    pcds_str s = {0, 0};
    if (i >= ev->argc) return s;
    for (u32 k = 0; k <= i; k++) s = pcds_string(&r);
    return s;
}

/* What your plugin implements. */
static void on_load(const u8 *state, u32 len, u32 version);
static i32 on_event(const pcds_ev *ev);
static u32 on_save(u8 *out, u32 cap, u32 *version);

/* Reads the payload of an event of `kind` into `ev`. 0 if it is whole, -1 if it ends too soon (the
 * fields of an event only grow at the end, so bytes left over are ignored). */
static inline int pcds_decode(u32 kind, const u8 *payload, u32 len, pcds_ev *ev) {
    pcds_reader r = {payload, len, 0};
    ev->kind = kind;
    switch (kind) {
    case PCDS_TICK:
        ev->game_ms = pcds_i64(&r); ev->tick = pcds_i32(&r); ev->players = pcds_u32(&r);
        ev->since_ms = pcds_u32(&r); ev->unix_ms = pcds_i64(&r);
        break;
    case PCDS_JOIN:
        ev->client = pcds_u64(&r); ev->name = pcds_string(&r); ev->steam_id = pcds_string(&r);
        ev->player_id = pcds_u64(&r);
        break;
    case PCDS_LEAVE:
        ev->client = pcds_u64(&r); ev->name = pcds_string(&r); ev->player_id = pcds_u64(&r);
        break;
    case PCDS_CHAT:
        ev->client = pcds_u64(&r); ev->name = pcds_string(&r); ev->text = pcds_string(&r);
        ev->player_id = pcds_u64(&r);
        break;
    case PCDS_COMMAND:
        ev->client = pcds_u64(&r); ev->name = pcds_string(&r); ev->command = pcds_string(&r);
        ev->argc = pcds_u32(&r);
        /* Each argument takes at least 4 bytes: a count the bytes cannot hold is a short payload. */
        if (ev->argc > r.left / 4) return -1;
        ev->args = r;
        for (u32 i = 0; i < ev->argc; i++) (void)pcds_string(&r);
        ev->text = pcds_string(&r); ev->player_id = pcds_u64(&r);
        break;
    case PCDS_RPC_BEFORE:
    case PCDS_RPC_AFTER:
        ev->client = pcds_u64(&r); ev->rpc = pcds_string(&r); ev->object = pcds_u64(&r);
        ev->args_json = pcds_string(&r);
        break;
    case PCDS_OBJECT_PLACED:
    case PCDS_OBJECT_DESTROYED:
        ev->id = pcds_i32(&r); ev->group = pcds_string(&r);
        for (int i = 0; i < 3; i++) ev->position[i] = pcds_f32(&r);
        ev->client = pcds_u64(&r);
        break;
    case PCDS_WORLD_SAVED:
        ev->game_ms = pcds_i64(&r); ev->ok = pcds_u8(&r) != 0;
        break;
    case PCDS_RULE_CHANGE:
        ev->changes_json = pcds_string(&r);
        break;
    case PCDS_HTTP_DONE:
        ev->ticket = pcds_u64(&r); ev->status = pcds_u32(&r); ev->error = pcds_string(&r);
        ev->body = pcds_string(&r);
        break;
    case PCDS_TIMER:
        ev->timer_id = pcds_u32(&r); ev->game_ms = pcds_i64(&r); ev->late_ms = pcds_u32(&r);
        break;
    case PCDS_FLIP:
        ev->game_ms = pcds_i64(&r); ev->server_version = pcds_string(&r);
        break;
    default:
        return -1;
    }
    return r.bad ? -1 : 0;
}

/* pcds_event: the server calls it for each event. Returns 0 (go on), or 1 (veto: only a
 * PCDS_RPC_BEFORE can be vetoed, for any other kind 1 counts as 0), or a negative number when the
 * payload is no good (the server counts a fault). A kind this header does not know is a newer
 * server's event: 0. */
PCDS_EXPORT("pcds_event") i32 pcds_event(u32 kind, const u8 *payload, u32 len) {
    if (kind < PCDS_TICK || kind > PCDS_FLIP) return 0;
    pcds_ev ev = {0};
    if (pcds_decode(kind, payload, len, &ev) != 0) return -1;
    i32 verdict = on_event(&ev);
    return (kind == PCDS_RPC_BEFORE && verdict == 1) ? 1 : 0;
}

/* pcds_load: once, after the start. `state` is 0 when nothing was saved; else the bytes that
 * on_save wrote, and the version it gave them. */
PCDS_EXPORT("pcds_load") i32 pcds_load(const u8 *state, u32 len, u32 version) {
    on_load(state, state ? len : 0, version);
    return 0;
}

/* pcds_save: the state to keep, at every world save, before a hot update and at a reload, as
 * (pointer << 32) | length; 0 for none. The server copies it and calls pcds_free. */
static u32 pcds_saved_version;
PCDS_EXPORT("pcds_save") u64 pcds_save(void) {
    u32 ptr = pcds_alloc(PCDS_SAVE_CAP);
    if (!ptr) return 0;
    u32 version = 0;
    u32 n = on_save((u8 *)(unsigned long)ptr, PCDS_SAVE_CAP, &version);
    if (n == 0 || n > PCDS_SAVE_CAP) { pcds_free(ptr, PCDS_SAVE_CAP); return 0; }
    /* Give back what was not used, so that the server's pcds_free(ptr, n) is the top of the stack. */
    pcds_top = ptr - (u32)(unsigned long)&pcds_arena[0] + n;
    pcds_saved_version = version;
    return ((u64)ptr << 32) | n;
}

/* pcds_state_version: the version of the bytes the last pcds_save gave. */
PCDS_EXPORT("pcds_state_version") u32 pcds_state_version(void) { return pcds_saved_version; }

/* Not exported, because both are optional: pcds_unload (the last call before the plugin is
 * switched off; export it the same way if you need it). */

#endif
