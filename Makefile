CC = clang
CFLAGS = -Wall -Wextra -O3 -std=gnu11
LDFLAGS = 

TARGET = utop
PREFIX ?= $(HOME)/.local
BINDIR = $(PREFIX)/bin

all: $(TARGET)

$(TARGET): src/main.c
	$(CC) $(CFLAGS) src/main.c -o $(TARGET) $(LDFLAGS)

install: $(TARGET)
	mkdir -p $(BINDIR)
	cp $(TARGET) $(BINDIR)/$(TARGET)
	chmod 755 $(BINDIR)/$(TARGET)

uninstall:
	rm -f $(BINDIR)/$(TARGET)

clean:
	rm -f $(TARGET)
