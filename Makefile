# The Planet Crafter dedicated server is one program, pcds (server/): the server and the
# command line that installs and runs it.
#
#   make build       server/target/release/pcds, with libsteam_api.so next to it
#   make install     ~/.local/bin/pcds, a link to that build
#   make test        the tests (PCDS_TEST_DATA: game tables to test on, else run/data/2.103)
#   make bench       the tick benchmark: make bench SAVE=run/worlds/world.json [PLAYERS=4]
#                    (PCDS_BENCH_SECONDS: the game time it runs, default 300)
#   make lint        clippy and shellcheck
#   make docker      the Docker image (IMAGE=...)
#   make docker-test the image's hot update, end to end (docker/test.sh; needs game tables:
#                    PCDS_TEST_DATA=DIR, else run/data/2.103)
#   make dist        the release tarball, as the release workflow makes it (VERSION=vX.Y.Z)
#   make clean

SHELL := /bin/bash
MANIFEST := --manifest-path server/Cargo.toml
RELEASE := server/target/release
# On a shared host, heavy commands run in agent-run's slice on 8 cores; elsewhere (CI) as they are.
LIMIT := $(if $(shell command -v agent-run),agent-run planetcrafter --cpus 8 --)
# libsteam_api.so in the steamworks crate's build output (shell, so it is looked up after the build).
STEAM_LIB_DIR := $$(dirname "$$(ls -t $(RELEASE)/build/steamworks-sys-*/out/libsteam_api.so | head -1)")
VERSION ?= v$(shell sed -n 's/^version = "\(.*\)"/\1/p' server/Cargo.toml | head -1)
DIST := pcds-$(VERSION)-linux-x86_64
IMAGE ?= planet-crafter-dedicated-server
SAVE ?= run/worlds/world.json
PLAYERS ?= 4

.PHONY: build install test bench lint docker docker-test dist clean

# The program finds libsteam_api.so next to itself; copied then renamed, so a running copy keeps its own.
build:
	$(LIMIT) cargo build $(MANIFEST) --release --locked
	cp "$(STEAM_LIB_DIR)/libsteam_api.so" $(RELEASE)/libsteam_api.so.new && mv -f $(RELEASE)/libsteam_api.so.new $(RELEASE)/libsteam_api.so

install: build
	mkdir -p $(HOME)/.local/bin
	ln -sfn $(abspath $(RELEASE)/pcds) $(HOME)/.local/bin/pcds

# Compiled first, then run in at most 6 GB of address space.
test: build
	$(LIMIT) cargo test $(MANIFEST) --release --locked --no-run
	ulimit -v 6000000 && LD_LIBRARY_PATH="$(STEAM_LIB_DIR)" $(LIMIT) cargo test $(MANIFEST) --release --locked

bench: build
	LD_LIBRARY_PATH="$(STEAM_LIB_DIR)" PCDS_BENCH_SAVE=$(abspath $(SAVE)) PCDS_RNG_SEED=1 PCDS_BENCH_PLAYERS=$(PLAYERS) \
	  $(LIMIT) cargo test $(MANIFEST) --release --locked bench -- --ignored --nocapture

lint:
	$(LIMIT) cargo clippy $(MANIFEST) --release --locked
	shellcheck install.sh docker/entrypoint.sh docker/test.sh

docker:
	$(LIMIT) docker build -t $(IMAGE) .

docker-test: docker
	PCDS_TEST_DATA="$${PCDS_TEST_DATA:-run/data/2.103}" docker/test.sh $(IMAGE)

dist: build
	rm -rf dist/$(DIST) && mkdir -p dist/$(DIST)/web
	cp $(RELEASE)/pcds $(RELEASE)/libsteam_api.so install.sh README.md LICENSE dist/$(DIST)/
	cp server/web/index.html dist/$(DIST)/web/
	tar -C dist -czf dist/$(DIST).tar.gz $(DIST)
	cd dist && sha256sum $(DIST).tar.gz > $(DIST).tar.gz.sha256

clean:
	cargo clean $(MANIFEST)
	rm -rf dist
