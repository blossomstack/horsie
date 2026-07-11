# horsie — common developer tasks.
# The CLI is two binaries: `horsie` (cli crate) spawns the sibling
# `horsie-runtime` (runtime crate), so build-cli builds both.

CARGO ?= cargo
PROFILE ?= release
ifeq ($(PROFILE),release)
  PROFILE_FLAG := --release
  TARGET_DIR := target/release
else
  PROFILE_FLAG :=
  TARGET_DIR := target/debug
endif

# Install location (override with PREFIX=/usr/local, BINDIR=..., or DESTDIR for
# staged installs). Defaults to the XDG user-local bin — no sudo required.
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin

.DEFAULT_GOAL := build-cli
.PHONY: build-cli build test fmt fmt-check clippy deny check ts-types install-cli uninstall-cli clean help

## build-cli: build the horsie CLI + its sandboxed runtime child ($(PROFILE))
build-cli:
	$(CARGO) build $(PROFILE_FLAG) -p cli -p runtime
	@echo "built: $(TARGET_DIR)/horsie  $(TARGET_DIR)/horsie-runtime"

## build: build the whole workspace
build:
	$(CARGO) build --workspace

## test: run the full test suite
test:
	$(CARGO) test --workspace

## fmt: format all code
fmt:
	$(CARGO) fmt --all

## fmt-check: verify formatting (CI)
fmt-check:
	$(CARGO) fmt --all -- --check

## clippy: lint with warnings denied (CI)
clippy:
	$(CARGO) clippy --all-targets --all-features -- -D warnings

## deny: supply-chain checks (requires cargo-deny)
deny:
	$(CARGO) deny check advisories bans licenses sources --all-features

## check: the full pre-PR gate (fmt + clippy + tests)
check: fmt-check clippy test

## ts-types: regenerate TypeScript protocol types from fluorite schemas (needs the
## `fluorite` CLI on PATH — `cargo install fluorite` — plus node/npm)
ts-types:
	cd clients/ts && npm install --no-audit --no-fund && npm run generate-types && npm run typecheck

## install-cli: build + install horsie and horsie-runtime into $(BINDIR)
install-cli: build-cli
	@mkdir -p "$(DESTDIR)$(BINDIR)"
	install -m 0755 "$(TARGET_DIR)/horsie" "$(DESTDIR)$(BINDIR)/horsie"
	install -m 0755 "$(TARGET_DIR)/horsie-runtime" "$(DESTDIR)$(BINDIR)/horsie-runtime"
	@echo "installed: $(DESTDIR)$(BINDIR)/horsie  $(DESTDIR)$(BINDIR)/horsie-runtime"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) echo "note: $(BINDIR) is not on your PATH — add it to run \`horsie\` directly";; esac

## uninstall-cli: remove horsie and horsie-runtime from $(BINDIR)
uninstall-cli:
	rm -f "$(DESTDIR)$(BINDIR)/horsie" "$(DESTDIR)$(BINDIR)/horsie-runtime"
	@echo "removed horsie + horsie-runtime from $(DESTDIR)$(BINDIR)"

## clean: remove build artifacts
clean:
	$(CARGO) clean

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## //'
