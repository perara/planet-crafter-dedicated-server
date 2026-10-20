# make build | check | scenario | package | install       (needs clang with the wasm32 target and wasm-ld)
build:
	clang --target=wasm32 -nostdlib -O2 -Wall -Wl,--no-entry -o plugin.wasm plugin.c

check: build
	pcds plugin check .

scenario: build
	pcds plugin test .

package: build
	pcds plugin pack .

install: build
	pcds plugin install . --force

.PHONY: build check scenario package install
