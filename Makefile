# october — common developer tasks.
# The CLI is two binaries: `october` (cli crate) spawns the sibling
# `october-runtime` (runtime crate), so build-cli builds both.

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
.PHONY: build-cli build test fmt fmt-check clippy deny check install-cli uninstall-cli clean help

## build-cli: build the october CLI + its sandboxed runtime child ($(PROFILE))
build-cli:
	$(CARGO) build $(PROFILE_FLAG) -p cli -p runtime
	@echo "built: $(TARGET_DIR)/october  $(TARGET_DIR)/october-runtime"

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

## install-cli: build + install october and october-runtime into $(BINDIR)
install-cli: build-cli
	@mkdir -p "$(DESTDIR)$(BINDIR)"
	install -m 0755 "$(TARGET_DIR)/october" "$(DESTDIR)$(BINDIR)/october"
	install -m 0755 "$(TARGET_DIR)/october-runtime" "$(DESTDIR)$(BINDIR)/october-runtime"
	@echo "installed: $(DESTDIR)$(BINDIR)/october  $(DESTDIR)$(BINDIR)/october-runtime"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) echo "note: $(BINDIR) is not on your PATH — add it to run \`october\` directly";; esac

## uninstall-cli: remove october and october-runtime from $(BINDIR)
uninstall-cli:
	rm -f "$(DESTDIR)$(BINDIR)/october" "$(DESTDIR)$(BINDIR)/october-runtime"
	@echo "removed october + october-runtime from $(DESTDIR)$(BINDIR)"

## clean: remove build artifacts
clean:
	$(CARGO) clean

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## //'
