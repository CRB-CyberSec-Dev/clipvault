.PHONY: build test xvfb-test run deb clean

build:
	cargo build --workspace

test:
	cargo test -p clipvault-core

xvfb-test:
	./tests/xvfb_integration.sh

wayland-test:
	./tests/wayland_smoke.sh

run: build
	./target/debug/clipvault daemon

release:
	cargo build --release --workspace

deb: release
	cargo deb -p clipvault
	@ls -la target/debian/*.deb

clean:
	cargo clean
