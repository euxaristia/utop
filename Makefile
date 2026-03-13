TARGET = target/release/utop
PREFIX ?= $(HOME)/.local
BINDIR = $(PREFIX)/bin

all: $(TARGET)

$(TARGET): src/main.rs
	cargo build --release

install: $(TARGET)
	mkdir -p $(BINDIR)
	cp $(TARGET) $(BINDIR)/utop
	chmod 755 $(BINDIR)/utop

uninstall:
	rm -f $(BINDIR)/utop

clean:
	cargo clean
