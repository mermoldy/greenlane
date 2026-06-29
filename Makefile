# greenlane — build
TARGET    := x86_64-unknown-linux-musl
BIN       := greenlane

.PHONY: web build run cross-setup linux clean

web: ## Build the viewer bundle (embedded into the binary via rust-embed)
	cd web && bun install && bun run build

build: web ## Release build for the host platform
	cargo build --release

run: web ## Debug build + run (pass ARGS=…, e.g. ARGS="attach 1234 --serve 127.0.0.1:8080")
	cargo run -- $(ARGS)

cross-setup: ## Install cross (one-time)
	cargo install cross --git https://github.com/cross-rs/cross

linux: web ## Cross-compile a static Linux binary
	cross build --release --target $(TARGET)

clean: ## Remove build artifacts
	cargo clean
	rm -rf web/dist/* web/node_modules
