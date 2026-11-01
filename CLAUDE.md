# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A macOS-focused **modded fork of Alacritty** (upstream `0.18.0-dev`, Rust edition 2024, MSRV 1.85). The git repo lives in this `alacritty/` directory (the parent folder is just a wrapper). Upstream commits are the base; local mods sit as uncommitted changes/untracked files on top.

Local mods vs. upstream:
- App/bundle renamed to **Alac** (`Alac.app`, `Alac.dmg`) in `Makefile`; template lives in untracked `extra/osx/Alac.app/` (the upstream `extra/osx/Alacritty.app` template files are deleted).
- Custom tab bar replacing native macOS tabs: `alacritty/src/display/tab_bar.rs`, `alacritty/src/macos/tabs.rs`.
- Native macOS menu: `alacritty/src/macos/menu.rs`.
- File-explorer side panel showing the terminal's cwd: `alacritty/src/display/file_explorer.rs`.
- Related hooks in `event.rs`, `input/mod.rs`, `display/mod.rs`, `config/bindings.rs`, `config/color.rs`, `renderer/*`; extra `objc2-app-kit`/`objc2-foundation` features in `alacritty/Cargo.toml`; config docs in `extra/man/alacritty.5.scd`.

## Commands

```sh
cargo build                      # debug build
cargo run                        # run the terminal
cargo build --release
cargo test                       # whole workspace (CI default)
cargo test -p alacritty_terminal --no-default-features   # CI also runs this
cargo test -p alacritty_config_derive
cargo test <name>                # single test by name filter
cargo clippy --all-targets       # CI lint; `clippy` cfg denies warnings
cargo +nightly fmt               # rustfmt.toml uses nightly-only options
make app                         # build target/release/osx/Alac.app (needs scdoc, tic, codesign)
make dmg                         # Alac.dmg;  make binary-universal / app-universal for x86_64+aarch64
```

`make app` needs `scdoc` to compile man pages in `extra/man/*.scd`. Sets `MACOSX_DEPLOYMENT_TARGET=10.12`.

## Architecture

Cargo workspace, 4 crates:

- **`alacritty_terminal`** — UI-independent terminal core, publishable lib. `term/` (state machine, cells, search, colors), `grid/` (storage + scrollback), `tty/` (PTY per platform), `event_loop.rs` (dedicated PTY I/O thread feeding the VTE parser into `Term`), `selection.rs`, `vi_mode.rs`, `sync.rs` (`FairMutex` guarding `Term` shared between the I/O thread and the UI thread).
- **`alacritty`** — the app binary. Winit event loop + OpenGL (glutin) rendering.
  - `main.rs` → `cli.rs` (clap; also IPC `msg` subcommands over a unix socket) → `event.rs` (`Processor`, central event dispatch/state) → `window_context.rs` (one per window: `Term`, PTY event loop, `Display`, config).
  - `display/` — window, size info, damage tracking, cursor, hints, content iteration (`content.rs`), plus the mod UI (tab bar, file explorer).
  - `renderer/` — GL text renderer (glyph atlas in `text/`, GLSL vs GLES2 paths), rect renderer (`rects.rs`) used for UI chrome.
  - `input/` — keyboard/mouse handling and binding resolution; `config/` — serde-based TOML config (with live reload via `monitor.rs`), bindings, `migrate/` for YAML→TOML.
  - `macos/` — platform glue via `objc2`; `polling/`, `scheduler.rs` (timer-based event scheduling), `message_bar.rs`.
- **`alacritty_config`** / **`alacritty_config_derive`** — `ConfigDeserialize` derive macro and config helpers that give partial/replace-on-error deserialization semantics; `alacritty_config_derive/tests` holds its tests. New config fields in `alacritty/src/config/*` use this derive; document them in `extra/man/alacritty.5.scd`.

Cargo features `x11`/`wayland` (default) only matter on Linux/BSD; `main.rs` fails compilation if none enabled on non-mac/non-Windows.

## Engine boundary

Keep the mods independent of the terminal engine so it could be swapped later (libghostty-vt is still alpha, no tagged release). `display/tab_bar.rs`, `display/file_explorer.rs`, `macos/*` and `config/{explorer,tab_bar}.rs` must not use `Term`, `Grid`, `Cell`, selection, search or `tty`. The only allowed import is `alacritty_terminal::index::{Column, Point}`, used as drawing coordinates for the renderer. Engine access belongs in the hooks in `event.rs`.

## Conventions

- `rustfmt.toml` is strict (max-width heuristics, `imports_granularity = "Module"`, comment width 100) — run nightly fmt.
- Crate roots `#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]`.
- Notable user-visible changes go in `CHANGELOG.md` (upstream convention, see `CONTRIBUTING.md`).
