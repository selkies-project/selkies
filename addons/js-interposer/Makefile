PREFIX ?= /usr

all:
	gcc -shared -fPIC -o selkies_joystick_interposer.so joystick_interposer.c -ldl -pthread

# The multiarch subdirectory is what `/usr/$LIB` expands to at load time, so
# LD_PRELOAD resolves the interposer for both 64-bit and 32-bit processes
LIBDIR = $(PREFIX)/lib/$(shell gcc -print-multiarch | sed -e 's/i.*86/i386/')

install: all
	mkdir -p $(LIBDIR)
	cp *.so $(LIBDIR)/

clean:
	rm -f *.so
