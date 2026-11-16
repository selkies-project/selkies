/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * libxkbcommon as a Wayland application uses it: a keymap the seat delivered,
 * the modifier state the seat reports, a delivered key resolved against both,
 * and a keysym's character.
 *
 *   xkb_lookup
 *
 * One request per line on standard input, one answer per line:
 *   keymap PATH        -> 1 when the keymap loaded, else 0
 *   mods D L K G       -> 1 (depressed, latched, locked modifiers and group)
 *   sym EVDEV          -> the keysym the evdev key resolves to (0 without a keymap)
 *   char KEYSYM        -> the Unicode code point it types (0 for none)
 * Built and run by tests/lib/keys.ts.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xkbcommon/xkbcommon.h>

int main(void)
{
    struct xkb_context *ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    struct xkb_keymap *keymap = NULL;
    struct xkb_state *state = NULL;
    char line[4096];
    while (fgets(line, sizeof line, stdin)) {
        line[strcspn(line, "\n")] = '\0';
        unsigned long a, b, c, d;
        if (strncmp(line, "keymap ", 7) == 0) {
            FILE *f = fopen(line + 7, "rb");
            struct xkb_keymap *loaded = NULL;
            if (f) {
                loaded = xkb_keymap_new_from_file(ctx, f, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
                fclose(f);
            }
            if (loaded) {
                if (state)
                    xkb_state_unref(state);
                if (keymap)
                    xkb_keymap_unref(keymap);
                keymap = loaded;
                state = xkb_state_new(keymap);
            }
            printf("%d\n", loaded != NULL);
        } else if (sscanf(line, "mods %lu %lu %lu %lu", &a, &b, &c, &d) == 4) {
            if (state)
                xkb_state_update_mask(state, a, b, c, 0, 0, d);
            printf("1\n");
        } else if (sscanf(line, "sym %lu", &a) == 1) {
            printf("%u\n", state ? xkb_state_key_get_one_sym(state, (xkb_keycode_t)(a + 8)) : 0);
        } else if (sscanf(line, "char %lu", &a) == 1) {
            printf("%u\n", xkb_keysym_to_utf32((xkb_keysym_t)a));
        } else {
            printf("?\n");
        }
        fflush(stdout);
    }
    if (state)
        xkb_state_unref(state);
    if (keymap)
        xkb_keymap_unref(keymap);
    xkb_context_unref(ctx);
    return 0;
}
