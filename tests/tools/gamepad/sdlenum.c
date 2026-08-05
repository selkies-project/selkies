#include <SDL2/SDL.h>
#include <stdio.h>
int main(void) {
    if (SDL_Init(SDL_INIT_JOYSTICK | SDL_INIT_GAMECONTROLLER) < 0) { printf("SDL_Init: %s\nRESULT joysticks=-1\n", SDL_GetError()); return 1; }
    int n = SDL_NumJoysticks();
    for (int i = 0; i < n; i++) printf("  [%d] %s\n", i, SDL_JoystickNameForIndex(i));
    printf("RESULT joysticks=%d\n", n);
    SDL_Quit(); return 0;
}
