/* hello-c: a plugin for the Planet Crafter dedicated server, written in C.
 *
 *   !hello   greets the player and says how often the plugin was asked, on this world
 *   !roll    rolls a die, 1 to 6
 *
 * It shows what a module needs besides the language: the state (a counter that survives restarts
 * and hot updates, version 1), the commands, and a source of randomness. A module has no clock and
 * no random numbers of its own. What it gets are the events: here a small xorshift generator is
 * stirred with the player ids at joins, and with the game time at every tick (every ten seconds).
 */
#include "pcds.h"

static u32 hellos;                         /* saved: how often !hello was used */
static u64 rng = 0x9E3779B97F4A7C15ull;    /* xorshift64 state; never 0 */

/* Stirs a number into the generator. */
static void stir(u64 v) {
    rng ^= (v + 0x9E3779B97F4A7C15ull) * 0xBF58476D1CE4E5B9ull;
    rng ^= rng >> 31;
    if (rng == 0) rng = 0x9E3779B97F4A7C15ull;
}

/* A number from 1 to 6. */
static u32 roll(void) {
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    return (u32)((rng >> 32) % 6u) + 1u;
}

/* State version 1 is one u32, the counter. A version this plugin does not know starts empty. */
static void on_load(const u8 *state, u32 len, u32 version) {
    hellos = 0;
    if (state && version == 1) {
        pcds_reader r = {state, len, 0};
        u32 n = pcds_u32(&r);
        if (!r.bad) hellos = n;
    } else if (state) {
        pcds_log(1, "unknown state version, starting empty", 36);
    }
}

static u32 on_save(u8 *out, u32 cap, u32 *version) {
    if (cap < 4) return 0;
    for (int i = 0; i < 4; i++) out[i] = (u8)(hellos >> (8 * i));
    *version = 1;
    return 4;
}

static void reply(u64 client, pcds_text *t) {
    pcds_tell(client, t->p, t->len);
}

static i32 on_event(const pcds_ev *ev) {
    char buf[200];
    pcds_text t = {buf, 0, sizeof buf};
    switch (ev->kind) {
    case PCDS_JOIN:
        stir(ev->player_id);
        stir(ev->client);
        break;
    case PCDS_TICK:
        stir((u64)ev->game_ms);
        break;
    case PCDS_COMMAND:
        if (pcds_str_is(ev->command, "hello")) {
            if (hellos < 0xFFFFFFFFu) hellos++;
            pcds_puts(&t, "Hello ");
            pcds_put(&t, ev->name.p, ev->name.len);
            pcds_puts(&t, " from C! I have said hello ");
            pcds_putu(&t, hellos);
            pcds_puts(&t, hellos == 1 ? " time." : " times.");
            reply(ev->client, &t);
        } else if (pcds_str_is(ev->command, "roll")) {
            stir(ev->client);
            pcds_put(&t, ev->name.p, ev->name.len);
            pcds_puts(&t, " rolled a ");
            pcds_putu(&t, roll());
            pcds_puts(&t, ".");
            reply(ev->client, &t);
        }
        break;
    default:
        break;
    }
    return 0;
}
