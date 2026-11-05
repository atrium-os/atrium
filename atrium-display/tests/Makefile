# Host-side tests for the shared EDID parser: make -C atrium-display/tests
CFLAGS ?= -O2 -g
CFLAGS += -Wall -Wextra -Werror -std=c11

test: test_edid
	./test_edid

test_edid: test_edid.c ../edid.c ../sink.c ../edid.h ../display_hw.h desktop_monitor_edid.h
	$(CC) $(CFLAGS) -o $@ test_edid.c ../edid.c ../sink.c

clean:
	rm -f test_edid

.PHONY: test clean
