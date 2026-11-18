PREFIX ?= $(HOME)/.local
BINDIR = $(abspath $(PREFIX)/bin)
APPDIR = $(PREFIX)/share/applications
ICONDIR = $(PREFIX)/share/icons/hicolor/scalable/apps

.PHONY: build install uninstall

build:
	cargo build --release

install: build
	install -Dm755 target/release/screensaver $(BINDIR)/screensaver
	install -d $(APPDIR)
	sed 's|@BINDIR@|$(BINDIR)|' screensaver.desktop > $(APPDIR)/screensaver.desktop
	install -Dm644 screensaver.svg $(ICONDIR)/screensaver.svg

uninstall:
	rm -f $(BINDIR)/screensaver $(APPDIR)/screensaver.desktop $(ICONDIR)/screensaver.svg
