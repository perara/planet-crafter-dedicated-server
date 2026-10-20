/* {id}: a plugin for the Planet Crafter dedicated server, written in C (docs/plugins-guide.md).
 * It answers !hello, and counts how often it was asked: the count survives restarts and hot updates. */
#include "pcds.h"

static u32 greeted;

static void on_load(const u8 *state, u32 len, u32 version) {
    greeted = 0;
    if (state && version == 1 && len >= 4) { pcds_reader r = {state, len, 0}; greeted = pcds_u32(&r); }
}

static void on_command(u64 client, pcds_str name, pcds_str command, pcds_reader *args, u32 argc) {
    (void)args; (void)argc;
    if (!pcds_str_is(command, "hello")) return;
    greeted++;
    char buf[200];
    pcds_text t = {buf, 0, sizeof buf};
    pcds_puts(&t, "Hello ");
    pcds_put(&t, name.p, name.len);
    pcds_puts(&t, " from C! I have said hello ");
    pcds_putu(&t, greeted);
    pcds_puts(&t, " times.");
    pcds_tell(client, t.p, t.len);
}

static u32 on_save(u8 *out, u32 cap, u32 *version) {
    if (cap < 4) return 0;
    for (int i = 0; i < 4; i++) out[i] = (u8)(greeted >> (8 * i));
    *version = 1;
    return 4;
}
