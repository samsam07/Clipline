.PHONY: build release build-windows release-windows install test test-live clean

# Where `make install` puts clipline. ~/.local/bin is on PATH by default on Fedora;
# override with `make install BINDIR=/some/dir`.
BINDIR ?= $(HOME)/.local/bin

build:
	cargo build

# Release build — the self-contained binary (target/release/clipline[.exe]).
release:
	cargo build --release

# Cross-compile clipline.exe for Windows from Linux via the MinGW toolchain (as Vox does).
# Needs `rustup target add x86_64-pc-windows-gnu` and mingw64-gcc. No extra flags: the exe
# imports only DLLs that ship with Windows (msvcrt, kernel32, ole32, …), so it stays
# self-contained (P1-B) — `.cargo/config.toml`'s static CRT applies to MSVC builds only.
WIN_TARGET := x86_64-pc-windows-gnu

# Debug build: target/x86_64-pc-windows-gnu/debug/clipline.exe.
build-windows:
	cargo build --target $(WIN_TARGET) -p clipline
	@echo "built target/$(WIN_TARGET)/debug/clipline.exe"

# Release build — the one to copy to a Windows box:
# target/x86_64-pc-windows-gnu/release/clipline.exe.
release-windows:
	cargo build --release --target $(WIN_TARGET) -p clipline
	@echo "built target/$(WIN_TARGET)/release/clipline.exe"

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
