/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * The one GLX entry point the X servers' GLX code takes from libGL itself, loaded from the
 * host at run time beside libepoxy, which answers for every GL function: the software DRI
 * provider hands the driver glXGetProcAddressARB to find GL with. Where the host has no
 * libGL it answers NULL, which leaves that provider without a driver, as on a host without
 * Mesa, and the server without software GLX rather than unable to start.
 */

#include <dlfcn.h>
#include <pthread.h>
#include <stddef.h>

typedef void (*gl_proc)(void);

static void *libgl;
static pthread_once_t libgl_once = PTHREAD_ONCE_INIT;

static void
open_libgl(void)
{
    libgl = dlopen("libGL.so.1", RTLD_NOW | RTLD_LOCAL);
}

static gl_proc
forward(const char *entry, const unsigned char *name)
{
    gl_proc (*fn)(const unsigned char *);

    pthread_once(&libgl_once, open_libgl);
    if (!libgl)
        return NULL;
    fn = (gl_proc (*)(const unsigned char *)) dlsym(libgl, entry);
    return fn ? fn(name) : NULL;
}

gl_proc glXGetProcAddressARB(const unsigned char *name);
gl_proc glXGetProcAddress(const unsigned char *name);

gl_proc
glXGetProcAddressARB(const unsigned char *name)
{
    return forward("glXGetProcAddressARB", name);
}

gl_proc
glXGetProcAddress(const unsigned char *name)
{
    return forward("glXGetProcAddress", name);
}
