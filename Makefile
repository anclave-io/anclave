# Anclave — developer entry points.
#
# `make ci` runs exactly what CI runs, in the same order, so a green local run
# means a green pull request. Anything CI checks and this file does not is a
# gap that will only show up after you push.

CARGO ?= cargo
INSTALL_DIR ?= $(HOME)/.local/bin
SOCKET ?= /tmp/anclaved.sock

.DEFAULT_GOAL := help
.PHONY: help build release test fmt fmt-check clippy lint ci containment \
        workflows scripts daemon cli tui install-local clean

help: ## Show this help
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk -F':.*?## ' '{printf "  \033[1m%-14s\033[0m %s\n", $$1, $$2}'

## --- what CI runs -------------------------------------------------------

ci: fmt-check clippy test containment ## Everything CI runs, in CI's order

fmt-check: ## Fail if anything is unformatted
	$(CARGO) fmt --all -- --check

clippy: ## Lint with warnings as errors
	$(CARGO) clippy --all-targets --all-features -- -D warnings

test: ## Run the whole test suite
	$(CARGO) test --workspace

# The containment tests skip silently when no container runtime can start one,
# which looks identical to passing. CI asserts they ran; so does this.
containment: ## Prove containment against a real container runtime
	@out=$$($(CARGO) test -p anclaved --test containment -- --nocapture 2>&1); \
	echo "$$out" | grep -E '^test result|runtimes exercised' || true; \
	echo "$$out" | grep -q 'runtimes exercised: \[\]' && { \
		echo "error: no container runtime available — containment went unchecked" >&2; \
		exit 1; \
	}; \
	echo "$$out" | grep -qE '^test result: ok' || exit 1

## --- the other half CI checks -------------------------------------------

workflows: ## Lint GitHub workflows (semantics, not just YAML)
	@command -v actionlint >/dev/null 2>&1 && actionlint -color || \
		podman run --rm -v "$$PWD:/mnt:ro" -w /mnt docker.io/rhysd/actionlint:latest -color

scripts: ## Check the shell scripts are POSIX-clean
	@for f in scripts/*.sh; do \
		echo "--- $$f"; sh -n "$$f"; \
		command -v shellcheck >/dev/null 2>&1 && shellcheck -s sh "$$f" || \
			podman run --rm -v "$$PWD:/mnt:ro" -w /mnt docker.io/koalaman/shellcheck:stable -s sh "$$f"; \
	done

lint: fmt-check clippy workflows scripts ## Every linter

## --- building and running ------------------------------------------------

build: ## Debug build
	$(CARGO) build --workspace

release: ## Optimised build of the three binaries
	$(CARGO) build --release --bin anclaved --bin anclave --bin anclave-cli

fmt: ## Format in place
	$(CARGO) fmt --all

daemon: ## Run the daemon in the foreground
	$(CARGO) run --bin anclaved -- --socket $(SOCKET)

cli: ## Ask the daemon what containment this host can provide
	ANCLAVE_SOCKET=$(SOCKET) $(CARGO) run --quiet --bin anclave-cli -- daemon sandbox

tui: ## Run the terminal client (a preview — no colour, no streaming)
	ANCLAVE_SOCKET=$(SOCKET) $(CARGO) run --bin anclave

install-local: release ## Install the built binaries into INSTALL_DIR
	@mkdir -p $(INSTALL_DIR)
	@for b in anclaved anclave anclave-cli; do \
		install -m 755 target/release/$$b $(INSTALL_DIR)/$$b && echo "installed $$b"; \
	done

clean: ## Remove build output
	$(CARGO) clean
