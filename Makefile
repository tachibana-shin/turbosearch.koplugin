# Makefile for turbosearch.
#
# One entry point for everything that is not `cargo`. The shell scripts that used
# to sit in script/ were folded in here: the tasks were small enough that a Makefile
# says more than a script per task, and there is now one place to see all of them
# (`make help`) rather than filenames to guess at.
#
# The compiler and the input-injection helper stay as scripts, because they are
# programs rather than steps: one is invoked by several targets, and the other is a
# Python program with a command line of its own.
#
# ## Where the plugin comes from
#
# There is no `plugin/` directory in the repository. Everything KOReader loads is
# generated into `build/turbosearch.koplugin/` from the Teal sources in `tl/`, and
# the whole directory is a build output. The alternative — a plugin directory under
# version control, holding Lua that is also generated from `tl/` — put the generated
# files one commit away from the sources they are compiled from, and it took a
# `git rm` of three Lua files to notice how much of a plugin is not source.

# The Teal compiler. script/tl pins a version and its checksum, and downloads it on
# first use, so this is the same compiler everywhere.
TL := ./script/tl
TL_FLAGS := --include-dir tl --gen-target 5.1

# The plugin directory, generated.
PLUGIN := build/turbosearch.koplugin

# Every Teal source, and the Lua each compiles to inside the plugin directory.
#
# `main` and the settings screen are the plugin. `_meta` is the metadata the reader
# shows. `turbosearch` is the FFI binding.
TEAL := tl/turbosearch.tl tl/plugin.tl tl/turbosearch_settings.tl tl/meta.tl
GENERATED := $(PLUGIN)/turbosearch.lua $(PLUGIN)/main.lua \
             $(PLUGIN)/turbosearch_settings.lua $(PLUGIN)/_meta.lua

# Platforms, and the target each is built for.
#
# Kindle Paperwhite 3 is the target the whole design is built around: ARMv7 with a
# hard-float ABI. x86_64 is here because it is what the desktop reader on the
# maintainer's machine runs, and it is the only way to try the plugin without a
# device. aarch64 covers the newer readers, and costs nothing to keep.
HOST_TARGET ?= $(shell rustc -vV 2>/dev/null | sed -n 's/^host: //p')
DESKTOP_TARGET = x86_64-unknown-linux-gnu
KINDLE_TARGET = armv7-unknown-linux-gnueabihf
AARCH64_TARGET = aarch64-unknown-linux-gnu
PLATFORMS = desktop kindle aarch64

VERSION ?= $(shell git describe --tags --abbrev=0 2>/dev/null || echo 0.0.0)

.DEFAULT_GOAL := help
.PHONY: help check lint test fmt build clean tl lua-check lua-build lua-test \
        plugin plugin-arm packages package clean-scratch install-probe

help: ## What each target does.
	@echo "turbosearch — see note.md for the design"
	@echo
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo "  install         point KOReader at the plugin built for this machine"
	@echo
	@echo "Everything except the engine and the plugin needs no library on the"
	@echo "machine; the tests that want one are ignored and opt in via"
	@echo "TURBOSEARCH_LIBRARY."

check: lint test lua-check lua-build ## Everything CI runs, in order.
	@echo "all checks pass"

lint: fmt clippy ## The two lints.

fmt: ## Format.
	cargo fmt --all --check

clippy: ## Clippy, with warnings promoted.
	cargo clippy --all-targets -- -D warnings

test: ## Rust tests, no library needed.
	cargo test --workspace

test-library: ## Rust tests that decode real books. Needs TURBOSEARCH_LIBRARY.
	TURBOSEARCH_LIBRARY="$${TURBOSEARCH_LIBRARY:?set TURBOSEARCH_LIBRARY}" \
		cargo test --workspace -- --ignored

build: ## Engine and command-line tools for this machine.
	cargo build --release

tl: ## The Teal compiler, pinned and verified. Downloads on first use.
	$(TL) --version

lua-check: tl ## Type-check the Teal sources.
	$(TL) check --include-dir tl $(TEAL)

# `-c` fails the build on a type error rather than writing Lua that will not load,
# which is the difference between a mistake caught here and a plugin that opens and
# then does nothing.
lua-build: lua-check ## Compile the Teal into the plugin directory.
	@mkdir -p $(PLUGIN)
	$(TL) gen -c $(TL_FLAGS) -o $(PLUGIN)/turbosearch.lua tl/turbosearch.tl
	$(TL) gen -c $(TL_FLAGS) -o $(PLUGIN)/main.lua tl/plugin.tl
	$(TL) gen -c $(TL_FLAGS) -o $(PLUGIN)/turbosearch_settings.lua tl/turbosearch_settings.tl
	$(TL) gen -c $(TL_FLAGS) -o $(PLUGIN)/_meta.lua tl/meta.tl
	@if command -v luajit >/dev/null 2>&1; then \
		for f in $(GENERATED); do luajit -bl $$f /dev/null; done; \
	fi

# The engine is chosen by architecture at run time, so both libraries sit in the
# plugin directory under different names and `jit.arch` picks. An x86-64 library
# reaching a Kindle would fail at ffi.load with a message about the ELF class, and
# deciding which by name is what keeps that from being a search that returns nothing.
# Each engine target also depends on lua-build, which is idempotent, so asking for
# all three of them does not compile the Teal three times.
engine-desktop: lua-build
	@mkdir -p target/$(DESKTOP_TARGET)/release
	cargo build --release --target $(DESKTOP_TARGET)
	cp target/$(DESKTOP_TARGET)/release/libturbosearch.so $(PLUGIN)/

# zig cc rather than a cross gcc, because this machine has no arm- or aarch64-gcc,
# and zig builds for both targets and many more from the same toolchain. The
# results are plain ELF for the target, not something that needs zig to run.
engine-kindle: lua-build
	@mkdir -p target/$(KINDLE_TARGET)/release
	cargo zigbuild --release --target $(KINDLE_TARGET)
	cp target/$(KINDLE_TARGET)/release/libturbosearch.so \
		$(PLUGIN)/libturbosearch-armv7.so

engine-aarch64: lua-build
	@mkdir -p target/$(AARCH64_TARGET)/release
	cargo zigbuild --release --target $(AARCH64_TARGET)
	cp target/$(AARCH64_TARGET)/release/libturbosearch.so \
		$(PLUGIN)/libturbosearch-aarch64.so

# `make plugin` with no argument builds for the machine it is run on, which is the
# one place the plugin can be tried without a device.
plugin: lua-check ## Build the plugin for a platform. `make plugin PLATFORM=kindle`.
	$(MAKE) engine-$(or $(PLATFORM),desktop)

# One archive per platform. Each is self-contained — the plugin's Lua, the metadata,
# and the engine library built for that platform — because the alternative, one
# archive holding every architecture, would ship four libraries of which three are
# dead weight on any given reader.
# One archive per platform. Each is self-contained — the plugin's Lua, the metadata,
# and the engine library built for that platform — because the alternative, one
# archive holding every architecture, would ship three libraries of which two are
# dead weight on any given reader.
packages: engine-desktop engine-kindle engine-aarch64 ## One archive per platform.
	@for platform in $(PLATFORMS); do \
		case $$platform in \
			desktop) target=$(DESKTOP_TARGET);; \
			kindle)  target=$(KINDLE_TARGET);; \
			aarch64) target=$(AARCH64_TARGET);; \
		esac; \
		stage=build/dist/turbosearch-$$platform; \
		rm -rf $$stage; mkdir -p $$stage; \
		cp $(PLUGIN)/*.lua $$stage/; \
		cp target/$$target/release/libturbosearch.so \
			$$stage/libturbosearch.so; \
		(cd $$stage && zip -qr ../../turbosearch-$$platform-$(VERSION).zip .); \
		echo "build/turbosearch-$$platform-$(VERSION).zip"; \
	done
	$(MAKE) manifest

# A machine-readable index of the release: which archive is which platform, the
# target it was built for, and a checksum of each. Attached alongside the archives
# so a script — or a reader deciding whether to redeploy — can tell what it has
# without unpacking anything.
manifest: ## Write build/turbosearch.json, an index of the archives.
	@VERSION=$(VERSION) python3 script/release_manifest.py build/turbosearch.json

package: packages ## Alias, for a release check.

clean: ## Build artifacts: cargo's target, the plugin, and the release archives.
	cargo clean
	rm -rf build/
	rm -f *.idx *.idx-wal *.idx-shm

clean-scratch: ## Indexes left over from measuring.
	rm -f *.idx *.idx-wal *.idx-shm

lua-test: lua-build ## The plugin against stubbed KOReader widgets.
	@if [ ! -f "$(PLUGIN)/libturbosearch.so" ]; then \
		cp target/$(DESKTOP_TARGET)/release/libturbosearch.so $(PLUGIN)/ \
			2>/dev/null || \
		cp target/release/libturbosearch.so $(PLUGIN)/ 2>/dev/null || true; \
	fi
	TS_TEST_INDEX="$${TS_TEST_INDEX:-/tmp/opencode/e2e.idx}" luajit tests/plugin_smoke.lua

install: plugin ## Point KOReader at the plugin built for this machine.
	@ln -sfn "$(CURDIR)/$(PLUGIN)" \
		$${KOREADER_PLUGIN_DIR:-$$HOME/.config/koreader/plugins}/turbosearch.koplugin
	@echo "linked into $${KOREADER_PLUGIN_DIR:-$$HOME/.config/koreader/plugins}"

install-probe: ## Copy the reader-inspection probe into ~/.config/koreader.
	@mkdir -p $${KOREADER_PLUGIN_DIR:-$$HOME/.config/koreader/plugins}/turbosearch-probe.koplugin
	cp tests/koreader-probe/*.lua \
		$${KOREADER_PLUGIN_DIR:-$$HOME/.config/koreader/plugins}/turbosearch-probe.koplugin/
	@echo "probe installed"

run: build ## Search a phrase over an existing index, from the command line.
	cargo run --release --bin latency -- $${DB:-/tmp/opencode/e2e.idx} $${QUERY:-"thám tử"}
