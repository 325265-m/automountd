PREFIX ?= /usr/local
CARGO ?= cargo
DESTDIR ?=
BINDIR := $(DESTDIR)$(PREFIX)/bin
UNITDIR := $(DESTDIR)/etc/systemd/system
CONFDIR := $(DESTDIR)/etc/automountd

.PHONY: all build test check lint install uninstall clean

all: build

build:
	$(CARGO) build --release

test:
	$(CARGO) test --all-targets

check:
	$(CARGO) check --all-targets

lint:
	$(CARGO) clippy --all-targets

install: build
	install -Dm755 target/release/automountd $(BINDIR)/automountd
	install -d -m755 $(UNITDIR)
	sed "s|/usr/local/bin/automountd|$(BINDIR)/automountd|g" packaging/automountd.service > $(UNITDIR)/automountd.service
	chmod 644 $(UNITDIR)/automountd.service
	install -Dm644 README.md $(DESTDIR)$(PREFIX)/share/doc/automountd/README.md
	install -d -m755 $(CONFDIR)
	@echo "Binary and systemd unit installed. Next steps:"
	@echo "  sudo automountd config init"
	@echo "  sudo systemctl daemon-reload && sudo systemctl enable --now automountd"

uninstall:
	-rm -f $(BINDIR)/automountd
	-rm -f $(UNITDIR)/automountd.service
	-rmdir $(CONFDIR) 2>/dev/null || true
	-systemctl daemon-reload

clean:
	$(CARGO) clean
