# horsie — common developer tasks.
# Three binaries: `horsie` (cli crate) spawns the sibling `horsie-runtime`
# (runtime crate) per job, so build-cli builds both. `horsie-server` (server
# crate) is the standalone session server, independent of the CLI — build it
# with build-server.

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
.PHONY: build-cli build-server build test fmt fmt-check clippy deny check ts-types web web-build install-cli uninstall-cli install-server uninstall-server clean help

## build-cli: build the horsie CLI + its sandboxed runtime child ($(PROFILE))
build-cli:
	$(CARGO) build $(PROFILE_FLAG) -p horsie -p horsie-runtime
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

## web: run the web UI dev server (needs bun + a running `horsie-server`)
web:
	cd clients/web && bun install && bun run generate-types && bun run dev

## web-build: typecheck + production-build the web UI (needs bun)
web-build:
	cd clients/web && bun install && bun run generate-types && bun run build

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

## build-server: build the horsie-server binary ($(PROFILE))
build-server:
	$(CARGO) build $(PROFILE_FLAG) -p horsie-server
	@echo "built: $(TARGET_DIR)/horsie-server"

## install-server: build + install horsie-server into $(BINDIR)
install-server: build-server
	@mkdir -p "$(DESTDIR)$(BINDIR)"
	install -m 0755 "$(TARGET_DIR)/horsie-server" "$(DESTDIR)$(BINDIR)/horsie-server"
	@echo "installed: $(DESTDIR)$(BINDIR)/horsie-server"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) echo "note: $(BINDIR) is not on your PATH — add it to run \`horsie-server\` directly";; esac

## uninstall-server: remove horsie-server from $(BINDIR)
uninstall-server:
	rm -f "$(DESTDIR)$(BINDIR)/horsie-server"
	@echo "removed horsie-server from $(DESTDIR)$(BINDIR)"

## clean: remove build artifacts
clean:
	$(CARGO) clean

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## //'
