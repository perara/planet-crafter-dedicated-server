# make build | test | check | scenario | package | install       (one time: rustup target add wasm32-unknown-unknown)
CRATE := {crate}

build:
	cargo build --target wasm32-unknown-unknown --release
	cp target/wasm32-unknown-unknown/release/$(CRATE).wasm plugin.wasm

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
