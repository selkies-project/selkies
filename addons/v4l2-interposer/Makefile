PREFIX ?= /usr
# Assigned rather than defaulted: make predefines CC as `cc`, which may be a
# compiler without -print-multiarch. A command-line CC= still wins.
CC = gcc
CFLAGS ?= -O2

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
