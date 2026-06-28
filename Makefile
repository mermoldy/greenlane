# greenlane — build & deploy
TARGET    := x86_64-unknown-linux-musl
NS        := sbabak
POD       := scalr-server-0
CONTAINER := scalr
BIN       := greenlane
PID       ?= 1402
PORT      ?= 8080

.PHONY: web build run cross-setup linux deploy remote remote-stop clean

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

remote-stop: ## Stop any prior remote greenlane + local port-forward (frees the port)
	# In-pod server. pkill skips itself; the [g] bracket avoids matching this command.
	-kubectl exec -n $(NS) -c $(CONTAINER) $(POD) -- pkill -f '[g]reenlane attach'
	# Local helpers left over from a previous `make remote` (the streamed exec and
	# the port-forward), matched without self-matching via the [k] bracket trick.
	-pkill -f '[k]ubectl exec.*$(POD).*$(BIN)'
	-pkill -f '[k]ubectl port-forward.*$(POD).*$(PORT)'

remote: remote-stop ## Run the server on the pod and port-forward it locally (PID=… PORT=… to override)
	# remote-stop first clears the old process so the pod's $(PORT) is free; give it
	# a moment to release the socket before binding again.
	sleep 1
	kubectl exec -n $(NS) -c $(CONTAINER) $(POD) -- /tmp/$(BIN) attach $(PID) --serve 0.0.0.0:$(PORT) & \
	sleep 2; \
	kubectl port-forward -n $(NS) $(POD) $(PORT):$(PORT)

clean: ## Remove build artifacts
	cargo clean
	rm -rf web/dist/* web/node_modules
