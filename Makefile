.PHONY: build release install test test-live clean

# Where `make install` puts clipline. ~/.local/bin is on PATH by default on Fedora;
# override with `make install BINDIR=/some/dir`.
BINDIR ?= $(HOME)/.local/bin

build:
	cargo build

# Release build — the self-contained binary (target/release/clipline[.exe]).
release:
	cargo build --release

# Linux: build the release binary and copy it onto PATH, replacing any older copy. Run it
# again after every change to update — it is a copy, not a link, so it survives
# `make clean`. A running clipline keeps the old build until it is restarted.
install: release
	install -Dm755 target/release/clipline $(BINDIR)/clipline
	@$(BINDIR)/clipline --help

# `--all-features`: three core offer/promise tests need the `mock` feature (PLAN.md M2).
test:
	cargo test --all-features

# The live-compositor adapter tests: need a Wayland data-control session and OVERWRITE
# the clipboard (TEST.md W0).
test-live:
	cargo test -p clipline -- --ignored

clean:
	cargo clean
