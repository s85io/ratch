PREFIX ?= /usr/local
DESTDIR ?=
CARGO ?= cargo
BIN = target/release/ratch

.PHONY: all build check install uninstall clean

all: build

build:
	$(CARGO) build --release

check: build
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) test
	sh tests/test.sh "$(CURDIR)/$(BIN)"

install: build
	install -d $(DESTDIR)$(PREFIX)/bin $(DESTDIR)$(PREFIX)/share/man/man1
	install -m 755 $(BIN) $(DESTDIR)$(PREFIX)/bin/ratch
	install -m 644 ratch.1 $(DESTDIR)$(PREFIX)/share/man/man1/ratch.1
	install -d $(DESTDIR)$(PREFIX)/share/bash-completion/completions
	install -m 644 completions/ratch.bash $(DESTDIR)$(PREFIX)/share/bash-completion/completions/ratch
	install -d $(DESTDIR)$(PREFIX)/share/zsh/site-functions
	install -m 644 completions/_ratch $(DESTDIR)$(PREFIX)/share/zsh/site-functions/_ratch
	install -d $(DESTDIR)$(PREFIX)/share/fish/vendor_completions.d
	install -m 644 completions/ratch.fish $(DESTDIR)$(PREFIX)/share/fish/vendor_completions.d/ratch.fish

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/ratch \
	      $(DESTDIR)$(PREFIX)/share/man/man1/ratch.1 \
	      $(DESTDIR)$(PREFIX)/share/bash-completion/completions/ratch \
	      $(DESTDIR)$(PREFIX)/share/zsh/site-functions/_ratch \
	      $(DESTDIR)$(PREFIX)/share/fish/vendor_completions.d/ratch.fish

clean:
	$(CARGO) clean
