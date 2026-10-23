# GNU Make, including GNU Make on Windows. Native compiler/SDK must be installed.
# TARGET selects a Rust target; it does not install a cross C toolchain or SDK.
CARGO ?= cargo
RUSTUP ?= rustup
TARGET ?=
LINUX_TARGET ?= x86_64-unknown-linux-gnu
MACOS_TARGET ?= aarch64-apple-darwin
WINDOWS_TARGET ?= x86_64-pc-windows-msvc
TARGET_FLAG = $(if $(strip $(TARGET)),--target $(TARGET),)
CARGO_FLAGS ?= --locked

.PHONY: help check fmt test build release clean target-install build-linux build-macos build-windows release-linux release-macos release-windows

help:
	@echo "make check             Format check and clippy for all workspace targets"
	@echo "make test              Run workspace tests on the host (or TARGET runner)"
	@echo "make build             Debug build for host or TARGET"
	@echo "make release           Release build for host or TARGET"
	@echo "make build-linux       Build LINUX_TARGET (default x86_64-unknown-linux-gnu)"
	@echo "make build-macos       Build MACOS_TARGET (default aarch64-apple-darwin)"
	@echo "make build-windows     Build WINDOWS_TARGET (default x86_64-pc-windows-msvc)"
	@echo "make target-install TARGET=...  Add Rust standard library, not a C toolchain"
	@echo "Override target: make release TARGET=x86_64-apple-darwin"

fmt:
	$(CARGO) fmt --all --check

check: fmt
	$(CARGO) clippy $(CARGO_FLAGS) --workspace --all-targets $(TARGET_FLAG) -- -D warnings

test:
	$(CARGO) test $(CARGO_FLAGS) --workspace $(TARGET_FLAG)

build:
	$(CARGO) build $(CARGO_FLAGS) --workspace $(TARGET_FLAG)

release:
	$(CARGO) build $(CARGO_FLAGS) --release -p tinyimg-cli $(TARGET_FLAG)

build-linux:
	$(MAKE) build TARGET=$(if $(strip $(TARGET)),$(TARGET),$(LINUX_TARGET))

build-macos:
	$(MAKE) build TARGET=$(if $(strip $(TARGET)),$(TARGET),$(MACOS_TARGET))

build-windows:
	$(MAKE) build TARGET=$(if $(strip $(TARGET)),$(TARGET),$(WINDOWS_TARGET))

release-linux:
	$(MAKE) release TARGET=$(if $(strip $(TARGET)),$(TARGET),$(LINUX_TARGET))

release-macos:
	$(MAKE) release TARGET=$(if $(strip $(TARGET)),$(TARGET),$(MACOS_TARGET))

release-windows:
	$(MAKE) release TARGET=$(if $(strip $(TARGET)),$(TARGET),$(WINDOWS_TARGET))

target-install:
	$(if $(strip $(TARGET)),$(RUSTUP) target add $(TARGET),$(error Set TARGET to a Rust target triple))

clean:
	$(CARGO) clean
