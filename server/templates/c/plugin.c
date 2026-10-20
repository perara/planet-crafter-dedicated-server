/* {id}: a plugin for the Planet Crafter dedicated server, written in C (docs/plugins-guide.md).
 * It answers !hello, and counts how often it was asked: the count survives restarts and hot updates. */
#include "pcds.h"

static u32 greeted;

/* The state is one u32, version 1. No state, or a version this plugin does not know: start at 0. */
static void on_load(const u8 *state, u32 len, u32 version) {
    greeted = 0;
    if (state && version == 1) {
        pcds_reader r = {state, len, 0};
        u32 n = pcds_u32(&r);
        if (!r.bad) greeted = n;
    }
}

static i32 on_event(const pcds_ev *ev) {
    if (ev->kind != PCDS_COMMAND || !pcds_str_is(ev->command, "hello")) return 0;
    greeted++;
    char buf[200];
    pcds_text t = {buf, 0, sizeof buf};
    pcds_puts(&t, "Hello ");
    pcds_put(&t, ev->name.p, ev->name.len);
    pcds_puts(&t, " from C! I have said hello ");
    pcds_putu(&t, greeted);
    pcds_puts(&t, greeted == 1 ? " time." : " times.");
    pcds_tell(ev->client, t.p, t.len);
    return 0;
}

static u32 on_save(u8 *out, u32 cap, u32 *version) {
    if (cap < 4) return 0;
    for (int i = 0; i < 4; i++) out[i] = (u8)(greeted >> (8 * i));
    *version = 1;
    return 4;
}
