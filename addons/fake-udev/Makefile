TARGET_SO = libudev.so.1
TARGET_BASENAME = libudev.so
LIB_VERSION ?= 1.0.0-fake
FULL_TARGET = $(TARGET_BASENAME).$(LIB_VERSION)
CC = gcc
CFLAGS = -Wall -Wextra -fPIC -DPIC -O2 -g -pthread
LDFLAGS = -shared -ldl -pthread
LDFLAGS += -Wl,--version-script=libudev.sym
LDFLAGS += -Wl,-soname,$(TARGET_SO)
SOURCES = fake-libudev-core.c
OBJECTS = $(SOURCES:.c=.o)

# 32-bit build variables
TARGET_SO_32 = libudev_x86.so.1
TARGET_BASENAME_32 = libudev_x86.so
FULL_TARGET_32 = $(TARGET_BASENAME_32).$(LIB_VERSION)
CFLAGS_32 = $(CFLAGS) -m32
LDFLAGS_32 = -shared -Wl,--version-script=libudev.sym -m32 -ldl -pthread
LDFLAGS_32 += -Wl,-soname,$(TARGET_SO_32)
OBJECTS_32 = $(SOURCES:.c=.x86.o)

all: $(FULL_TARGET) $(TARGET_SO) $(TARGET_BASENAME)

all32: $(FULL_TARGET_32) $(TARGET_SO_32) $(TARGET_BASENAME_32)

$(FULL_TARGET): $(OBJECTS) libudev.h libudev.sym
	$(CC) $(LDFLAGS) -o $@ $(OBJECTS)
	@echo "Built $@"
$(TARGET_SO): $(FULL_TARGET)
	@echo "Creating SONAME symlink $(TARGET_SO) -> $(FULL_TARGET)"
	@ln -sf $(FULL_TARGET) $(TARGET_SO)
$(TARGET_BASENAME): $(TARGET_SO)
	@echo "Creating symlink $(TARGET_BASENAME) -> $(TARGET_SO)"
	@ln -sf $(TARGET_SO) $(TARGET_BASENAME)

# 32-bit build targets
$(FULL_TARGET_32): $(OBJECTS_32) libudev.h libudev.sym
	$(CC) $(LDFLAGS_32) -o $@ $(OBJECTS_32)
	@echo "Built $@"

$(TARGET_SO_32): $(FULL_TARGET_32)
	@echo "Creating SONAME symlink $(TARGET_SO_32) -> $(FULL_TARGET_32)"
	@ln -sf $(FULL_TARGET_32) $(TARGET_SO_32)

$(TARGET_BASENAME_32): $(TARGET_SO_32)
	@echo "Creating symlink $(TARGET_BASENAME_32) -> $(TARGET_SO_32)"
	@ln -sf $(TARGET_SO_32) $(TARGET_BASENAME_32)

%.o: %.c libudev.h
	$(CC) $(CFLAGS) -c -o $@ $<

%.x86.o: %.c libudev.h
	$(CC) $(CFLAGS_32) -c -o $@ $<

clean:
	rm -f $(OBJECTS) $(OBJECTS_32) $(FULL_TARGET) $(TARGET_SO) $(TARGET_BASENAME) $(FULL_TARGET_32) $(TARGET_SO_32) $(TARGET_BASENAME_32) core.* *.core

.PHONY: all all32 clean
