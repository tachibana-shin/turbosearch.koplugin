# Makefile for turbosearch.
#
# One entry point for everything that is not `cargo`. The shell scripts that used
# to sit in script/ were folded in here: the tasks were small enough that a Makefile
# says more than a script per task, and there is now one place to see all of them
# (`make help`) rather than filenames to guess at.
#
# The compiler, the release-index writer and the input-injection helper stay as
# scripts, because they are programs rather than steps: the first is invoked by
# several targets, and the others have a command line of their own.
#
# ## Where the plugin comes from
#
# There is no `plugin/` directory in the repository. Everything KOReader loads is
# generated into `build/turbosearch.koplugin/` from the Teal sources in `tl/`, and
# the whole directory is a build output. The alternative — a plugin directory under
# version control, holding Lua that is also generated from `tl/` — put the generated
# files one commit away from the sources they came from.

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
# One binary, one name: `libturbosearch.so`. A release is built per platform, each
# holding that file built for that platform, and LuaJIT loads it. There are no
# per-architecture filenames and nothing is chosen at run time — which binary to ship
# is the job of whoever picks the platform, not of the code that loads it. LuaJIT was
# never a candidate for that job either: it reports `arm` for soft- and hard-float ARM
# alike, so having it tell them apart would be a guess at the reader's firmware.
#
# The set is KOReader's own, because a plugin is loaded *into* the reader rather than
# run beside it, so the ABI of the .so has to be the ABI of the process that is already
# running. Each platform matches a `TARGET` in koreader-base's Makefile.defs.
#
# Three targets were tried and ruled out, each by a full build, so the next person does
# not spend the fifteen minutes again:
#
#   `arm-unknown-linux-musleabi` builds and then emits nothing. Cargo drops the
#   `cdylib` crate type for it — the target is for static binaries. It is where
#   rakuyomi's soft-float Kindle build goes, and rakuyomi ships static binaries that
#   call each other over a Unix socket, where the ABI changes nothing. That was the
#   suggestion here once too, and it was dropped: a socket daemon on an e-reader is a
#   permanent process and a protocol to maintain, to solve a problem the linker
#   setting already solves.
#
#   `arm-unknown-linux-gnueabi` under `cross` fails inside the container, in a
#   proc-macro: `can't find crate for zerofrom_derive`. That crate is a real
#   dependency — idna, through url, through sqlx-core — so it cannot be dropped. The
#   same target builds locally with zig as the linker, which is why `zigbuild` is
#   below rather than cross.
#
#   musl generally is not what KOReader uses. Every Kindle target in koreader-base's
#   Makefile.defs is a `*-linux-gnueabi[hf]` toolchain, and cross-glibc portability is
#   handled there with `GLIBC_VERSION_MAX`, not by swapping libc.

HOST_TARGET ?= $(shell rustc -vV 2>/dev/null | sed -n 's/^host: //p')

DESKTOP_TARGET = x86_64-unknown-linux-gnu
# Both ARM ABIs, because which one a Kindle wants is a property of its firmware, and
# the only way to be right on both is to ship both.
KINDLE_SF_TARGET = arm-unknown-linux-gnueabi
KINDLE_HF_TARGET = armv7-unknown-linux-gnueabihf
AARCH64_TARGET = aarch64-unknown-linux-gnu
MACOS_TARGET = aarch64-apple-darwin

PLATFORMS = desktop kindle kindlehf aarch64

VERSION ?= $(shell git describe --tags --abbrev=0 2>/dev/null || echo 0.0.0)

.DEFAULT_GOAL := help
.PHONY: help check lint test fmt build clean tl lua-check lua-build lua-test \
        plugin packages manifest clean-scratch install install-probe run

help: ## What each target does.
	@echo "turbosearch — see note.md for the design"
	@echo
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo "  install        point KOReader at the plugin built for this machine"
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

# The ARM libraries are built with `cargo zigbuild`, using zig as the linker for a
# soft- and a hard-float gnu target that have no cross gcc installed here.
#
# The alternative was `cross`, which puts each target in a container carrying that
# target's sysroot. It works for most targets and failed for exactly the one needed:
# a proc-macro (`zerofrom_derive`, pulled in by idna through url through sqlx-core)
# cannot be built inside its container for `arm-unknown-linux-gnueabi`. zig is a linker
# only, so proc-macros build for the host as they always do, and the failure does not
# arise.
#
# A musl target was the other option, and it is not one: cargo drops the `cdylib` crate
# type for soft-float ARM musl, because that target is for static binaries. rakuyomi
# uses it, and ships a daemon over a Unix socket rather than a library.
#
# cross needs a container engine; podman is already here. The one variable that would
# otherwise be left to chance is set explicitly rather than assumed.
ZIGBUILD ?= cargo zigbuild
CROSS ?= CROSS_CONTAINER_ENGINE=podman cross

# Each engine target also depends on lua-build, which is idempotent, so asking for
# several of them does not compile the Teal more than once.
engine-desktop: lua-build
	cargo build --release --target $(DESKTOP_TARGET)
	@test -f target/$(DESKTOP_TARGET)/release/libturbosearch.so

# `--lib` only. The release is a library, and building the command-line tools for a
# device target was the part that failed: an ARM EABI helper (`__aeabi_uread4`,
# needed by the object crate the CLI tools link) is missing from what zig provides
# as a linker, while the library itself links fine. The tools are for the machine
# that runs the build, and `make build` builds them there.
engine-kindle: lua-build
	$(ZIGBUILD) --lib --release --target $(KINDLE_SF_TARGET)

engine-kindlehf: lua-build
	$(ZIGBUILD) --lib --release --target $(KINDLE_HF_TARGET)

engine-aarch64: lua-build
	$(ZIGBUILD) --lib --release --target $(AARCH64_TARGET)

# macOS builds on macOS. There is no cross for it worth the trouble, and the .so is
# for a reader somebody is running themselves.
engine-macos: lua-build
	cargo build --lib --release --target $(MACOS_TARGET)

# `make plugin` with no argument builds for the machine it runs on, which is the one
# place the plugin can be tried without a device.
plugin: lua-check ## Build for a platform. `make plugin PLATFORM=kindle`.
	$(MAKE) engine-$(or $(PLATFORM),desktop)
	@case $(or $(PLATFORM),desktop) in \
		desktop)  t=$(DESKTOP_TARGET);; \
		kindle)   t=$(KINDLE_SF_TARGET);; \
		kindlehf) t=$(KINDLE_HF_TARGET);; \
		aarch64)  t=$(AARCH64_TARGET);; \
		macos)    t=$(MACOS_TARGET);; \
	esac; \
	cp target/$$t/release/libturbosearch.so $(PLUGIN)/libturbosearch.so; \
	echo "built $(PLUGIN) for $(or $(PLATFORM),desktop)"

# One archive per platform. Each is self-contained — the plugin's Lua, the metadata,
# and the engine library that platform needs — because the alternative, one archive
# carrying every architecture, ships the rest as dead weight on any given reader.
packages: engine-desktop engine-kindle engine-kindlehf engine-aarch64 ## One archive per platform.
	@for platform in $(PLATFORMS); do \
		case $$platform in \
			desktop)  target=$(DESKTOP_TARGET);; \
			kindle)   target=$(KINDLE_SF_TARGET);; \
			kindlehf) target=$(KINDLE_HF_TARGET);; \
			aarch64)  target=$(AARCH64_TARGET);; \
		esac; \
		stage=build/dist/turbosearch-$$platform; \
		rm -rf $$stage; mkdir -p $$stage; \
		cp $(PLUGIN)/*.lua $$stage/; \
		cp target/$$target/release/libturbosearch.so $$stage/libturbosearch.so; \
		(cd $$stage && zip -qr ../../turbosearch-$$platform-$(VERSION).zip .); \
		echo "build/turbosearch-$$platform-$(VERSION).zip"; \
	done
	$(MAKE) manifest

# A machine-readable index of the release: which archive is which platform, the target
# it was built for, and a checksum of each. Attached alongside the archives so a
# script — or a reader deciding whether to redeploy — can tell what it has without
# unpacking anything.
manifest: ## Write build/turbosearch.json, an index of the archives.
	@VERSION=$(VERSION) python3 script/release_manifest.py build/turbosearch.json

lua-test: lua-build ## The plugin against stubbed KOReader widgets.
	@if [ ! -f "$(PLUGIN)/libturbosearch.so" ]; then \
		cp target/$(DESKTOP_TARGET)/release/libturbosearch.so $(PLUGIN)/ \
			2>/dev/null || cp target/release/libturbosearch.so $(PLUGIN)/ \
			2>/dev/null || true; \
	fi
	TS_TEST_INDEX="$${TS_TEST_INDEX:-/tmp/opencode/e2e.idx}" luajit tests/plugin_smoke.lua

clean: ## Build artifacts: cargo's target, the plugin, and the release archives.
	cargo clean
	rm -rf build/
	rm -f *.idx *.idx-wal *.idx-shm

clean-scratch: ## Indexes left over from measuring.
	rm -f *.idx *.idx-wal *.idx-shm

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
