# greenlane — build & deploy
TARGET    := x86_64-unknown-linux-musl
NS        := sbabak
POD       := scalr-server-0
CONTAINER := scalr
BIN       := greenlane

.PHONY: web build run cross-setup linux deploy remote clean

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

deploy: linux ## Build the Linux binary and copy it into the k8s pod
	kubectl cp -n $(NS) -c $(CONTAINER) target/$(TARGET)/release/$(BIN) $(POD):/tmp/$(BIN)

remote: ## run server on k8s pod
	kubectl exec -n $(NS) -c $(CONTAINER) $(POD) -- /tmp/$(BIN) attach 64 --serve 0.0.0.0:8080 & \
	kubectl port-forward -n sbabak pod/scalr-server-0 8080:8080

clean: ## Remove build artifacts
	cargo clean
	rm -rf web/dist/* web/node_modules
