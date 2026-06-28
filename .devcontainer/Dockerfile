# Linux dev environment for greenlane.
#
# Purpose: test the Linux-only paths from a non-Linux host — chiefly the
# `sys.remote_exec` (PEP 768) attach over ptrace, and (later) `perf_event_open`
# off-CPU/CPU sampling. The base ships rustup + a stable Rust toolchain (edition
# 2024) and the usual build tooling; we add bun, uv, and a few runtime utils.
FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# procps gives `pgrep`/`ps` (find the target PID); pkg-config is handy for native
# crates. The base already provides git + a C toolchain/linker.
RUN apt-get update \
 && export DEBIAN_FRONTEND=noninteractive \
 && apt-get install -y --no-install-recommends pkg-config procps \
 && apt-get clean && rm -rf /var/lib/apt/lists/*

# bun — builds and tests the embedded web viewer. Installed under /usr/local so it
# is on PATH for the non-root `vscode` user the container runs as.
ENV BUN_INSTALL=/usr/local/bun
ENV PATH=${BUN_INSTALL}/bin:${PATH}
RUN curl -fsSL https://bun.sh/install | bash

# uv — drives the pinned Python dev tools (ruff/ty/pytest/pre-commit) and fetches
# CPython 3.14, the version the target bootstraps require (sys.remote_exec /
# sys.monitoring). Installed into /usr/local/bin so every user sees it.
RUN curl -LsSf https://astral.sh/uv/install.sh \
      | env UV_INSTALL_DIR=/usr/local/bin INSTALLER_NO_MODIFY_PATH=1 sh
