/* Test-only preload that makes dlopen fail for the libraries named in
   HIDE_LIBS, a ':'-separated list of file name prefixes ("libEGL" hides
   libEGL.so.1 and libEGL_nvidia.so.0 alike), so the paths a program takes on a
   host without them run on one that has them. A hidden open fails the way a
   missing file does, with dlerror set. Libraries a program links (DT_NEEDED)
   are loaded before any dlopen and stay as they are. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>

static int hidden(const char *file)
{
    const char *list = getenv("HIDE_LIBS");
    if (!file || !list || !*list)
        return 0;
    const char *base = strrchr(file, '/');
    base = base ? base + 1 : file;
    while (*list) {
        const char *end = strchr(list, ':');
        size_t len = end ? (size_t)(end - list) : strlen(list);
        if (len && strncmp(base, list, len) == 0)
            return 1;
        if (!end)
            break;
        list = end + 1;
    }
    return 0;
}

void *dlopen(const char *file, int mode)
{
    static void *(*real)(const char *, int);
    if (!real)
        real = (void *(*)(const char *, int))dlsym(RTLD_NEXT, "dlopen");
    if (hidden(file))
        return real("/nonexistent/hidden-by-HIDE_LIBS.so", mode);
    return real(file, mode);
}
