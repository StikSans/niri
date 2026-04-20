# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

niri is a scrollable-tiling Wayland compositor written in Rust on top of [Smithay](https://github.com/Smithay/smithay). The repo is a Cargo workspace:

- `niri/` (root crate) — the compositor binary. Entry point: `src/main.rs`; main state machine: `src/niri.rs` (`Niri` / `State`).
- `niri-config/` — KDL config parser (via `knuffel`). Separate crate because its build is slow and it's reused by tools.
- `niri-ipc/` — types for the IPC protocol. **Not Rust-semver-stable**; external consumers pin an exact version.
- `niri-visual-tests/` — GTK/libadwaita dev tool that renders hard-coded layout scenarios with mock windows. Not shipped.

The Smithay dependency is pinned to a specific **git rev** in `Cargo.toml` (workspace.dependencies.smithay). Don't bump it casually — niri tracks unreleased Smithay closely and local Smithay changes are sometimes required.

## Common commands

```bash
# Build / run (default features = dbus, systemd, xdp-gnome-screencast).
cargo build
cargo run                 # runs nested when WAYLAND_DISPLAY or DISPLAY is set

# Lint / format. rustfmt requires NIGHTLY toolchain (CI enforces).
cargo clippy --all --all-targets
cargo +nightly fmt --all

# Tests — always use --all to include sub-crates.
cargo test --all --exclude niri-visual-tests

# Randomized proptest suite in src/layout/tests.rs is skipped by default (too slow).
# Enable it with RUN_SLOW_TESTS. CI runs with these knobs:
RUN_SLOW_TESTS=1 PROPTEST_CASES=200000 PROPTEST_MAX_GLOBAL_REJECTS=200000 \
  cargo test --release --all --exclude niri-visual-tests

# Single test (Wayland integration tests live in src/tests/, module per feature area):
cargo test --all --exclude niri-visual-tests -- tests::remove_output

# Validate a config file without launching the compositor.
cargo run -- validate -c path/to/config.kdl

# MSRV is pinned in Cargo.toml at workspace.package.rust-version = "1.85".
# CI checks it with: cargo +1.85.0 check --all-targets

# Visual test app (needs GTK + libadwaita):
cargo run -p niri-visual-tests

# Tracy profiling (on-demand — safe to daily-drive):
cargo build --release --features=profile-with-tracy-ondemand
```

System build deps (from `.github/workflows/ci.yml`): `libudev`, `libgbm`, `libxkbcommon`, `libegl-mesa`, `libwayland`, `libinput`, `libdbus`, `libsystemd`, `libseat`, `libpipewire-0.3`, `libpango`, `libdisplay-info`. `libinput ≥ 1.30` enables the `have_libinput_plugin_system` cfg (see `build.rs`).

Feature flags worth knowing:
- `dbus` — freedesktop/GNOME/Mutter interfaces, accessibility (AccessKit), power key.
- `systemd` / `dinit` — init-system integration (global env import, transient scopes). `systemd` implies `dbus`.
- `xdp-gnome-screencast` — PipeWire screencasting via `xdg-desktop-portal-gnome`.
- Disabling default features is supported; CI checks each combination individually.

## Architecture

### Event loop and state

niri is built on a single-threaded `calloop` event loop. `State` (in `src/niri.rs`) owns everything:
- `State.niri: Niri` — all compositor state (outputs, layout, clients, grabs, ipc server, config, dbus, etc.).
- `State.backend: Backend` — the rendering/input backend, one of `Tty`, `Winit`, `Headless` (`src/backend/`).

Every event loop iteration ends with `State::refresh_and_flush_clients()`. Outputs track redraw state in a small state machine (`RedrawState`: `Idle` → `Queued` → `WaitingForVBlank` / `WaitingForEstimatedVBlank`); see `docs/wiki/Development:-Redraw-Loop.md`. The estimated-vblank leg exists to throttle frame callbacks when redraw produces no damage.

### Backends

- `backend/tty.rs` — real session: DRM + GBM + udev + libseat + libinput. Handles hotplug, session resume, multi-GPU via Smithay's `MultiRenderer`.
- `backend/winit.rs` — nested-in-a-Wayland-window dev backend.
- `backend/headless.rs` — used by integration tests in `src/tests/`.

### Layout — the heart of niri

`src/layout/` is scrollable-tiling logic. The hierarchy is:

```
Layout → Monitor (per output) → Workspace → Column → Tile (wraps a Window)
                                         \-> FloatingSpace
```

Key invariants (documented in `src/layout/mod.rs` top comment and `docs/wiki/Development:-Design-Principles.md`):

1. **Opening/closing/resizing windows must not visually move the focused window.**
2. **Disconnect + reconnect of an output must not change the layout.** Each workspace remembers its *original output*; on reconnection it migrates back.
3. Creating or moving a window onto a workspace resets its *original output* to the current one (reduces surprise after rearrangements).
4. Actions apply **immediately** in state even when animations are still playing. Animations are started with duration 0 rather than being skipped, to keep code paths uniform.
5. Fullscreen is just a tile in the scroll layout — not a separate layer. The layer-shell top layer and floating windows hide only when the view is stationary on a focused fullscreen tile.

**Adding a new layout operation:** add a variant to the `Op` enum at `src/layout/tests.rs:408` so the proptest randomized suite exercises it. If it's a routine op, also add it to the `every_op` arrays below the enum. Without this, proptest coverage drifts.

`Layout` is generic over `LayoutElement` so that tests can substitute `TestWindow` (in `src/layout/tests.rs`) for real Wayland-backed `Mapped` windows. Preserve this abstraction when editing.

### Wayland handlers and protocols

- `src/handlers/` — `CompositorHandler`, `XdgShellHandler`, `LayerShellHandler`, etc. Implementations of Smithay traits. Most delegate into `Niri` methods.
- `src/protocols/` — custom Wayland protocol implementations not provided by Smithay: `foreign_toplevel`, `gamma_control`, `screencopy`, `output_management`, `mutter_x11_interop`, `virtual_pointer`, `ext_workspace`.
- `src/layer/`, `src/window/` — wrapper types (`Mapped`, `Unmapped`) that attach niri-specific state to Smithay's window/layer-surface objects.
- `src/dbus/` — DBus services (mutter-compat `DisplayConfig`/`ScreenCast`/`Introspect`, login1, locale1, etc.). Enabled only with `feature = "dbus"`.

### IPC

External clients (the `niri msg` CLI, status bars) talk over a Unix socket whose path is exposed via `$NIRI_SOCKET`. `src/ipc/server.rs` handles the socket inside the event loop; `src/ipc/client.rs` drives `niri msg`. Protocol types live in the `niri-ipc` crate and are shared.

### Input

`src/input/` routes libinput events through Smithay's seat abstraction. Each *grab* mode lives in its own file: interactive move (`move_grab.rs`), interactive resize (`resize_grab.rs`), pick-window / pick-color (for screenshots and IPC picks), overview touch grab, swipe gesture trackers. A grab owns pointer/keyboard/touch focus for its duration.

### Rendering

`src/render_helpers/` — pipeline bits on top of Smithay's `GlesRenderer`: custom shaders (`shaders/` subdir), offscreen framebuffers, shadows, borders, gradients, blur, background effects. `niri_render_elements!` macro defines render-element enums used throughout the layout/UI. Direct scanout is used when possible; offscreening is an eye-candy-specific fallback.

### UI

`src/ui/` — compositor-drawn UI elements: hotkey overlay, exit-confirm dialog, config-error notification, screenshot UI, screen transition, MRU (most-recently-used) window switcher.

### xwayland-satellite integration

niri no longer ships its own Xwayland; the `xwayland-satellite` process handles X11 clients. Wiring is in `src/utils/xwayland/`.

## Logging levels (strict)

From `docs/wiki/Development:-Developing-niri.md`:

- `error!` — **bugs**. Things you'd normally `unwrap()` but we recover so the session stays alive. If users see `ERROR`, it's always a niri bug.
- `warn!` — bad-but-possible. User config errors, odd hardware, etc.
- `info!` — important normal-operation messages. `RUST_LOG=niri=info` should not be noisy.
- `debug!` — less important normal-operation details. Hiding debug must not degrade UX.
- `trace!` — everything else; compiled **out** of release builds (`release_max_level_debug` in Cargo.toml).

Prefer `error!` + continue over panicking inside the event loop — a compositor crash brings down the whole session.

## Testing conventions

- Unit tests live next to the code they cover (most concentrated in `src/layout/` and `niri-config/`).
- Integration tests in `src/tests/` drive the compositor via a real Wayland server-client pair through `Fixture` (`src/tests/fixture.rs`), using the `Headless` backend. Add a new module to `src/tests/mod.rs` when introducing a feature area with odd client-server interactions.
- `insta` snapshot tests are used in `niri-config/` and a few other places; update with `cargo insta review`.
- `src/layout/tests.rs` contains the proptest randomized suite. It is **guarded by `RUN_SLOW_TESTS`** — if you forget to set it, the tests silently run 0 cases (see the `ProptestConfig { cases: 0, ... }` block).
- Fullscreen/layout/animation regressions often only reproduce under `--release` with many proptest cases. Mirror CI's invocation locally for significant layout changes.

## Code conventions

- `rustfmt.toml` sets `imports_granularity = "Module"` and `group_imports = "StdExternalCrate"` — these require nightly rustfmt.
- `clippy.toml` disables interior-mutability lints on `smithay::desktop::Window`, `smithay::output::Output`, and `wayland_server::backend::ClientId` (they're fine to use in hashmap keys, etc.).
- `[lints.clippy]` in root `Cargo.toml` allows `new_without_default` and `collapsible_match`.
- Release profile has `overflow-checks = true` — do not rely on silent wrapping.
- Tracy spans: instrument with `let _span = tracy_client::span!("name");` at the top of a function to make it show up in the profiler.
- Config options: when adding one, update the default config (`resources/default-config.kdl` — see `docs/wiki/Development:-Design-Principles.md` for the editorial rules on what belongs there) and the wiki page under `docs/wiki/Configuration:-*.md`.

## Docs

User/developer docs are **in this repo** under `docs/wiki/` (an mkdocs site; `uv` + `mkdocs build` from `docs/`). CI publishes them to the GitHub wiki on push to `main`. Niri-IPC rustdoc is also published. When changing user-visible behavior, update the matching wiki page and include a `Since` annotation for new config options.
