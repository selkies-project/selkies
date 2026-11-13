/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/*
 * libgbm's API, loaded from the host at run time: linked as a static libgbm.a in place of
 * the library itself, so a program of the bundle starts on a host with no libgbm at all.
 *
 * libgbm is the one library a GPU's driver has to be reached through and that only the host
 * can supply, the one beside its driver: a copy carried in the bundle would be a different
 * Mesa's, or the NVIDIA stack's own GBM would not be found. Every function forwards to the
 * host's `libgbm.so.1`, opened on the first call; where the host has none, gbm_create_device
 * returns NULL, which every caller already takes for "no GPU here" and falls back to software
 * on, and nothing else can be reached without a device. A function an older libgbm lacks
 * answers as if it failed, except the `*_with_modifiers2` entries, added in Mesa 21.3, which
 * fall back to the `*_with_modifiers` ones without their flags.
 */

#include <dlfcn.h>
#include <pthread.h>
#include <stddef.h>
#include <stdint.h>

#include <gbm.h>

static void *libgbm;
static pthread_once_t libgbm_once = PTHREAD_ONCE_INIT;

static void
open_libgbm(void)
{
    libgbm = dlopen("libgbm.so.1", RTLD_NOW | RTLD_LOCAL);
}

/* The host's `name`, or NULL where the host has no libgbm or that libgbm lacks it. */
static void *
resolve(const char *name)
{
    pthread_once(&libgbm_once, open_libgbm);
    return libgbm ? dlsym(libgbm, name) : NULL;
}

/* A forwarder returning `fallback` where the host has no `name`. The pointer is cached in a
 * static that two threads may both fill on a first call, with the same value either way. */
#define FORWARD(ret, name, params, args, fallback)            \
    ret name params                                           \
    {                                                         \
        static ret (*fn) params;                              \
        if (!fn)                                              \
            fn = (ret (*) params) resolve(#name);             \
        if (!fn)                                              \
            return fallback;                                  \
        return fn args;                                       \
    }

#define FORWARD_VOID(name, params, args)                      \
    void name params                                          \
    {                                                         \
        static void (*fn) params;                             \
        if (!fn)                                              \
            fn = (void (*) params) resolve(#name);            \
        if (fn)                                               \
            fn args;                                          \
    }

static const union gbm_bo_handle no_handle;

FORWARD(int, gbm_device_get_fd, (struct gbm_device *gbm), (gbm), -1)
FORWARD(const char *, gbm_device_get_backend_name, (struct gbm_device *gbm), (gbm), NULL)
FORWARD(int, gbm_device_is_format_supported,
        (struct gbm_device *gbm, uint32_t format, uint32_t flags), (gbm, format, flags), 0)
FORWARD(int, gbm_device_get_format_modifier_plane_count,
        (struct gbm_device *gbm, uint32_t format, uint64_t modifier), (gbm, format, modifier), -1)
FORWARD_VOID(gbm_device_destroy, (struct gbm_device *gbm), (gbm))
FORWARD(struct gbm_device *, gbm_create_device, (int fd), (fd), NULL)
FORWARD(struct gbm_bo *, gbm_bo_create,
        (struct gbm_device *gbm, uint32_t width, uint32_t height, uint32_t format, uint32_t flags),
        (gbm, width, height, format, flags), NULL)
FORWARD(struct gbm_bo *, gbm_bo_create_with_modifiers,
        (struct gbm_device *gbm, uint32_t width, uint32_t height, uint32_t format,
         const uint64_t *modifiers, const unsigned int count),
        (gbm, width, height, format, modifiers, count), NULL)
FORWARD(struct gbm_bo *, gbm_bo_import,
        (struct gbm_device *gbm, uint32_t type, void *buffer, uint32_t flags),
        (gbm, type, buffer, flags), NULL)
FORWARD(void *, gbm_bo_map,
        (struct gbm_bo *bo, uint32_t x, uint32_t y, uint32_t width, uint32_t height,
         uint32_t flags, uint32_t *stride, void **map_data),
        (bo, x, y, width, height, flags, stride, map_data), NULL)
FORWARD_VOID(gbm_bo_unmap, (struct gbm_bo *bo, void *map_data), (bo, map_data))
FORWARD(uint32_t, gbm_bo_get_width, (struct gbm_bo *bo), (bo), 0)
FORWARD(uint32_t, gbm_bo_get_height, (struct gbm_bo *bo), (bo), 0)
FORWARD(uint32_t, gbm_bo_get_stride, (struct gbm_bo *bo), (bo), 0)
FORWARD(uint32_t, gbm_bo_get_stride_for_plane, (struct gbm_bo *bo, int plane), (bo, plane), 0)
FORWARD(uint32_t, gbm_bo_get_format, (struct gbm_bo *bo), (bo), 0)
FORWARD(uint32_t, gbm_bo_get_bpp, (struct gbm_bo *bo), (bo), 0)
FORWARD(uint32_t, gbm_bo_get_offset, (struct gbm_bo *bo, int plane), (bo, plane), 0)
FORWARD(struct gbm_device *, gbm_bo_get_device, (struct gbm_bo *bo), (bo), NULL)
FORWARD(union gbm_bo_handle, gbm_bo_get_handle, (struct gbm_bo *bo), (bo), no_handle)
FORWARD(int, gbm_bo_get_fd, (struct gbm_bo *bo), (bo), -1)
FORWARD(uint64_t, gbm_bo_get_modifier, (struct gbm_bo *bo), (bo), 0)
FORWARD(int, gbm_bo_get_plane_count, (struct gbm_bo *bo), (bo), 0)
FORWARD(union gbm_bo_handle, gbm_bo_get_handle_for_plane, (struct gbm_bo *bo, int plane),
        (bo, plane), no_handle)
FORWARD(int, gbm_bo_get_fd_for_plane, (struct gbm_bo *bo, int plane), (bo, plane), -1)
FORWARD(int, gbm_bo_write, (struct gbm_bo *bo, const void *buf, size_t count), (bo, buf, count), -1)
FORWARD_VOID(gbm_bo_set_user_data,
             (struct gbm_bo *bo, void *data, void (*destroy_user_data)(struct gbm_bo *, void *)),
             (bo, data, destroy_user_data))
FORWARD(void *, gbm_bo_get_user_data, (struct gbm_bo *bo), (bo), NULL)
FORWARD_VOID(gbm_bo_destroy, (struct gbm_bo *bo), (bo))
FORWARD(struct gbm_surface *, gbm_surface_create,
        (struct gbm_device *gbm, uint32_t width, uint32_t height, uint32_t format, uint32_t flags),
        (gbm, width, height, format, flags), NULL)
FORWARD(struct gbm_surface *, gbm_surface_create_with_modifiers,
        (struct gbm_device *gbm, uint32_t width, uint32_t height, uint32_t format,
         const uint64_t *modifiers, const unsigned int count),
        (gbm, width, height, format, modifiers, count), NULL)
FORWARD(struct gbm_bo *, gbm_surface_lock_front_buffer, (struct gbm_surface *surface), (surface),
        NULL)
FORWARD_VOID(gbm_surface_release_buffer, (struct gbm_surface *surface, struct gbm_bo *bo),
             (surface, bo))
FORWARD(int, gbm_surface_has_free_buffers, (struct gbm_surface *surface), (surface), 0)
FORWARD_VOID(gbm_surface_destroy, (struct gbm_surface *surface), (surface))
FORWARD(char *, gbm_format_get_name, (uint32_t gbm_format, struct gbm_format_name_desc *desc),
        (gbm_format, desc), NULL)

struct gbm_bo *
gbm_bo_create_with_modifiers2(struct gbm_device *gbm, uint32_t width, uint32_t height,
                              uint32_t format, const uint64_t *modifiers,
                              const unsigned int count, uint32_t flags)
{
    static struct gbm_bo *(*fn)(struct gbm_device *, uint32_t, uint32_t, uint32_t,
                                const uint64_t *, const unsigned int, uint32_t);
    if (!fn)
        fn = (struct gbm_bo *(*)(struct gbm_device *, uint32_t, uint32_t, uint32_t,
                                 const uint64_t *, const unsigned int, uint32_t))
            resolve("gbm_bo_create_with_modifiers2");
    if (fn)
        return fn(gbm, width, height, format, modifiers, count, flags);
    return gbm_bo_create_with_modifiers(gbm, width, height, format, modifiers, count);
}

struct gbm_surface *
gbm_surface_create_with_modifiers2(struct gbm_device *gbm, uint32_t width, uint32_t height,
                                   uint32_t format, const uint64_t *modifiers,
                                   const unsigned int count, uint32_t flags)
{
    static struct gbm_surface *(*fn)(struct gbm_device *, uint32_t, uint32_t, uint32_t,
                                     const uint64_t *, const unsigned int, uint32_t);
    if (!fn)
        fn = (struct gbm_surface *(*)(struct gbm_device *, uint32_t, uint32_t, uint32_t,
                                      const uint64_t *, const unsigned int, uint32_t))
            resolve("gbm_surface_create_with_modifiers2");
    if (fn)
        return fn(gbm, width, height, format, modifiers, count, flags);
    return gbm_surface_create_with_modifiers(gbm, width, height, format, modifiers, count);
}
