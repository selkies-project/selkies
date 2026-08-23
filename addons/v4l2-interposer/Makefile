PREFIX ?= /usr
# Assigned rather than defaulted: make predefines CC as `cc`, which may be a
# compiler without -print-multiarch. A command-line CC= still wins.
CC = gcc
CFLAGS ?= -O2

# Frames can also be taken from a PipeWire Video/Source node (the pixelflux
# virtual camera publishes one); libpipewire is dlopen'ed at runtime, only the
# headers are needed to build that backend
PW_CFLAGS := $(shell pkg-config --cflags libpipewire-0.3 2>/dev/null)
# The flags must actually compile: a pkg-config answering for another prefix
# (a Python environment's, for the system .pc) points at headers that are not
# there, in which case the standard install paths are tried instead.
HASH := \#
pw_compiles = $(shell echo '$(HASH)include <pipewire/pipewire.h>' | $(CC) $(1) -fsyntax-only -x c - >/dev/null 2>&1 && echo yes)
ifneq ($(call pw_compiles,$(PW_CFLAGS)),yes)
PW_CFLAGS := -I/usr/include/pipewire-0.3 -I/usr/include/spa-0.2
ifneq ($(call pw_compiles,$(PW_CFLAGS)),yes)
PW_CFLAGS :=
endif
endif
ifneq ($(PW_CFLAGS),)
CFLAGS += -DHAVE_PIPEWIRE $(PW_CFLAGS)
else
$(info libpipewire headers not found: building without the PipeWire frame source)
endif

# The multiarch subdirectory is what `/usr/$LIB` expands to at load time, so
# LD_PRELOAD resolves the interposer for both 64-bit and 32-bit processes
LIBDIR = $(PREFIX)/lib/$(shell $(CC) -print-multiarch | sed -e 's/i.*86/i386/')
LIBDIR32 = $(PREFIX)/lib/$(shell $(CC) -m32 -print-multiarch 2>/dev/null | sed -e 's/i.*86/i386/')

all: selkies_v4l2_interposer.so

# 32-bit build for Wine/Steam and 32-bit browsers; needs gcc-multilib on amd64
all32: selkies_v4l2_interposer_x86.so

selkies_v4l2_interposer.so: v4l2_interposer.c
	$(CC) $(CFLAGS) -shared -fPIC -o $@ $< -ldl -pthread

selkies_v4l2_interposer_x86.so: v4l2_interposer.c
	$(CC) -m32 $(CFLAGS) -shared -fPIC -o $@ $< -ldl -pthread

install: all
	mkdir -p $(LIBDIR)
	cp selkies_v4l2_interposer.so $(LIBDIR)/

install32: all32
	mkdir -p $(LIBDIR32)
	cp selkies_v4l2_interposer_x86.so $(LIBDIR32)/selkies_v4l2_interposer.so

clean:
	rm -f *.so

.PHONY: all all32 install install32 clean
