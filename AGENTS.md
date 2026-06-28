# Repository Guidelines

## Agent Configuration

This file is the single source of truth for agent guidance. Tool-specific entry
points are symlinks back to it, so editing `AGENTS.md` updates every agent:

- `CLAUDE.md` → `AGENTS.md` (Claude Code reads `CLAUDE.md`).
- `.claude/plugins` → `../plugins` (the agent-neutral `plugins/` dir is the
  canonical plugin location; Claude reaches it under `.claude/`).

When adding support for another agent, symlink its expected filename to
`AGENTS.md` rather than copying content. Do not edit the symlinks directly.

## Project Shape

greenlane is a Rust CLI/server with an embedded Vite/React/WebGL viewer and
Python bootstrap scripts used inside target processes.

- Rust sources live in `src/`.
- Python unit tests live in `tests/`; the gevent demo / load generator lives in
  `bench/`.
- Frontend sources live in `web/src/`.
- Generated artifacts are ignored: `target/`, `web/dist/`, `web/node_modules/`,
  `.venv/`, `*.tsbuildinfo`, and `*.glr`.
- Local project plugins should live under `plugins/`; keep plugin-specific
  generated output ignored or under that plugin's own ignored build directory.

## Tooling

Use the pinned project tools instead of global equivalents:

- Python: `uv run ...`
- Frontend: `bun run --cwd web ...`
- Rust: `cargo ...`

Before handing off changes, run:

```sh
uv run pre-commit run --all-files
cargo test --locked
cargo build --locked
bun run --cwd web build
```

For the inner loop, prefer `cargo check --locked` as the fast compile gate —
`cargo build --locked` pulls in bundled native dependencies and can take a while
from a cold target dir, so reserve the full build for the pre-handoff run above
and for CI/release.

The pre-commit suite includes general file hygiene, `cargo fmt`, Ruff lint and
format, Prettier, markdownlint, TypeScript type checking, `ty`, pytest, Bun
tests, and lychee in offline mode.

## Development Notes

- Keep the binary self-contained: the web bundle is built into `web/dist/` and
  embedded by `rust-embed`.
- Avoid editing generated files directly. Regenerate them through the owning
  tool.
- Do not commit `.glr` recordings.
- Be careful with attach/injection code. `src/bootstrap.py` runs inside user
  target processes, so hot-path overhead and cleanup behavior matter.
- Preserve Unix-socket and remote-exec behavior across Linux and macOS unless a
  change explicitly targets one platform.

## Git Hygiene

- This repo often has concurrent local changes. Do not revert files you did not
  intentionally modify.
- Keep changes scoped to the requested work.
- If formatting hooks touch unrelated in-progress files, call that out clearly
  in your handoff.
