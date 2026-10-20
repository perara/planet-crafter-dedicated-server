# make build | test | check | scenario | package | install       (one time: rustup target add wasm32-unknown-unknown)
CRATE := {crate}
# Where cargo writes (CARGO_TARGET_DIR if you set one).
OUT := $(or $(CARGO_TARGET_DIR),target)/wasm32-unknown-unknown/release/$(CRATE).wasm

build:
	cargo build --target wasm32-unknown-unknown --release
	cp $(OUT) plugin.wasm

test:
	cargo test

check: build
	pcds plugin check .

scenario: build
	pcds plugin test .

package: build
	pcds plugin pack .

install: build
	pcds plugin install . --force

.PHONY: build test check scenario package install
