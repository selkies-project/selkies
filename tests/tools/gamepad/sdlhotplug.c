/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/* Watches SDL2 joystick hotplug for `argv[1]` seconds (default 10) and prints
 * one line per added or removed device with the time since start, then a
 * RESULT line with the counts: what a pad served after the application
 * started looks like to SDL's device discovery. */
#include <SDL2/SDL.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    int seconds = argc > 1 ? atoi(argv[1]) : 10;
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (SDL_Init(SDL_INIT_JOYSTICK) != 0) {
        printf("SDL_Init failed: %s\nRESULT added=-1 removed=-1\n", SDL_GetError());
        return 1;
    }
    printf("start joysticks=%d\n", SDL_NumJoysticks());
    Uint32 t0 = SDL_GetTicks();
    int added = 0, removed = 0;
    while ((int)(SDL_GetTicks() - t0) < seconds * 1000) {
        SDL_Event ev;
        while (SDL_PollEvent(&ev)) {
            if (ev.type == SDL_JOYDEVICEADDED) {
                added++;
                printf("+%u ms added index=%d name=%s\n", SDL_GetTicks() - t0, ev.jdevice.which,
                       SDL_JoystickNameForIndex(ev.jdevice.which));
            } else if (ev.type == SDL_JOYDEVICEREMOVED) {
                removed++;
                printf("+%u ms removed id=%d\n", SDL_GetTicks() - t0, ev.jdevice.which);
            }
        }
        SDL_Delay(20);
    }
    printf("RESULT added=%d removed=%d\n", added, removed);
    SDL_Quit();
    return 0;
}
