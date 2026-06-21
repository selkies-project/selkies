PREFIX ?= /usr

all:
	gcc -shared -fPIC -o selkies_joystick_interposer.so joystick_interposer.c -ldl -pthread

install: all
	mkdir -p $(PREFIX)/lib/$(gcc -print-multiarch | sed -e 's/i.*86/i386/')
	cp *.so $(PREFIX)/lib/$(gcc -print-multiarch | sed -e 's/i.*86/i386/')/
clean:
	rm -f *.so