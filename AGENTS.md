# Nucleotide repository guide

This guide is based on the checked-in workspace manifest, toolchain/CI configuration, packaging scripts, and current entrypoints. Keep it synchronized when those sources change.

## Project and toolchain

- This is a Rust 2024 Cargo workspace. `rust-toolchain.toml` pins Rust `1.96.0` and installs `clippy`, `rustfmt`, and `rust-src`; `Cargo.lock` is committed.
- `crates/nucleotide` is the application and composition root. Its default binary is `nucl`; it also builds `nucl-grammar`. `nucleotide-update-smoke` requires `--features update-smoke-test`.
- Other workspace crates provide types, events, logging, appearance, core/editor/LSP, UI, projects, environments, processes, workspaces, VCS, remote support, and terminal support. `vendor/helix-stdx` and `vendor/helix-view` are workspace members; the root `exclude` list covers `vendor/block-0.1.6`, `vendor/velopack-1.2.0`, and `vendor/zed`.
- GPUI is path-vendored under `vendor/zed`. Helix dependencies use the revision and patches in the root `Cargo.toml`; use those manifests and `Cargo.lock` rather than guessing dependency versions.
- `target/`, `runtime/`, `.helix/`, and packaging output are generated or ignored. Do not commit them. Runtime sources must contain at least `queries/` and `themes/` when a script or bundled app needs them.

Zig `0.15.2` is required to build the Ghostty-backed terminal. The Nix flake (`nix develop`, or `nix develop .#ci` for the CI environment) supplies the pinned Rust/Zig toolchain and native dependencies; Windows builds need Zig installed separately.

## Common commands

```text
cargo run -p nucleotide                         # run the app (default binary: nucl)
cargo build --workspace                         # build workspace targets
cargo build --release                            # release build
cargo build -p nucleotide --bins                 # nucl + nucl-grammar

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- --deny warnings
cargo check --workspace --all-targets --locked
./scripts/check-layering.sh
cargo deny check
```

The repository-local `.cargo/config.toml` supplies `--cfg tokio_unstable`, sets `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1`, and adds a large Windows linker stack. Preserve those settings unless a build is specifically being debugged. Automatic grammar fetching is disabled; use `nucl --grammar fetch` and `nucl --grammar build` (or the packaging scripts) when grammars are needed.

## Tests and CI behavior

Tests live beside their crate's source and under crate-level `tests/` directories. The normal CI test command is:

```text
cargo test --workspace --locked -- --skip tests::performance_tests --skip tests::integration_tests::tests::performance_tests
```

The Nix `cargo-test` check additionally skips the two command-session tests named:

```text
tests::command_session_runs_program_args_and_reports_exit_code
tests::command_session_try_exit_code_reports_finished_child
```

Run the Linux remote transport fixtures separately:

```text
cargo test -p nucleotide-remote --locked --test v5_external_fixtures --test v5_loopback_fixtures
```

`v5_external_fixtures` is Linux-only and exercises a real host-built helper through fake SSH/WSL launchers; the loopback fixture starts the helper and multiplexes file/process requests. CI does not use the normal Rust artifact cache on Windows because cached `libghostty-vt-sys` native artifacts can fail across Windows hosts with `STATUS_ILLEGAL_INSTRUCTION`.

For focused work, prefer `cargo test -p <crate> <filter>` and add tests next to the code being changed. The CI dependency jobs also use `cargo deny check` and `cargo +nightly-2026-07-01 udeps --all-targets --workspace`.

## Architecture boundaries

`./scripts/check-layering.sh` enforces the following lower-to-higher order:

```text
nucleotide-types
  -> nucleotide-events / nucleotide-logging
  -> nucleotide-core / nucleotide-appearance
  -> nucleotide-editor / nucleotide-lsp / nucleotide-ui
  -> nucleotide
```

- `nucleotide-types` has no internal dependencies. Its GPUI and Helix integrations are optional features; the no-default-features build must stay free of `gpui`, `helix-core`, `helix-view`, and `helix-term`.
- `nucleotide-editor` must not depend on `nucleotide-ui`. Keep editor rendering/editing logic in that crate and shared visual controls in `nucleotide-ui`.
- `nucleotide-events` contains cross-crate domain event types (`document`, `ui`, `workspace`, `lsp_events`, `completion`, `run`, and `terminal`). It is not an event bus crate.
- `nucleotide-core` contains Helix/GPUI bridges and capabilities; `nucleotide` integrates those services with the workspace, input, overlays, terminal, and remote flows.
- Run the layering script after changing cross-crate dependencies. For `nucleotide-types`, also run `cargo check -p nucleotide-types --no-default-features` and inspect its normal dependency tree.

## Events and input

The current application event path is:

```text
Helix hooks -> private HelixEvent channel -> Application -> GPUI Update events -> UI
```

- `crates/nucleotide-events/src/` defines domain event types; it has no current `EventBus`, `EventHandler`, `event_bus.rs`, or `v2` hierarchy.
- `crates/nucleotide/src/types.rs` defines the application-level `Update` enum. `Application` in `crates/nucleotide/src/application/mod.rs` implements `EventEmitter<Update>` and emits variants with `cx.emit(...)`.
- `crates/nucleotide-core/src/event_bridge.rs` registers Helix hooks and forwards a private `HelixEvent` through a `OnceLock`-backed unbounded channel. It is transport, not a general public event bus.
- Input coordination is in `crates/nucleotide/src/input_coordinator.rs`; UI focus/navigation primitives are in `nucleotide-ui::{focus, navigable}`. There is no current `global_input` module.
- `completion_v2` is the name of the UI completion module and is unrelated to a removed event-system `v2` layer.

Keep event handling and UI updates on the existing entity/async boundaries; do not add a parallel bus or polling path when an existing `Update` or Helix bridge is appropriate.

## UI conventions

- `nucleotide-ui` is the shared GPUI component crate. Prefer its exported controls (`Button`, `TextInput`, `ListItem`, `Picker`, `Prompt`, dialogs, menus, modal/overlay surfaces), layout helpers, completion popup helpers, and split/resize helpers before creating an app-local equivalent. Extend the shared component when the behavior is reusable.
- `Theme` and `UIConfig` are GPUI globals. Read them with `cx.global::<Theme>()` / `cx.global::<UIConfig>()`; create themes with `Theme::from_tokens(DesignTokens::dark())` or `DesignTokens::light()`.
- Use semantic values from `theme.tokens` and the size scale in `crates/nucleotide-ui/src/tokens`; avoid hardcoded colors and spacing for shared UI. `crates/nucleotide-ui/src/tokens/README.md` documents the token layers and utilities.
- Shared styling/traits are in `nucleotide-ui::styling` and `nucleotide-ui::traits`. Use runtime theme switching through `nucleotide_ui::theme_manager` and keep the GPUI global synchronized.
- Hand-written GPUI elements remain appropriate for specialized editor/terminal rendering, but they should still use semantic tokens and existing styling helpers.

## Packaging and runtime operations

- macOS: run `./scripts/bundle-mac.sh`, then open `Nucleotide.app`. The script builds or uses `target/release/nucl` and `nucl-grammar`, copies a Helix runtime, and can build/copy Linux remote helpers. Set `NUCL_REQUIRE_REMOTE_HELPERS=1` when both helpers are mandatory.
- Linux remote helpers: `./scripts/build-remote-helpers.sh` requires `cargo-zigbuild` and builds `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` artifacts named `nucleotide-remote-linux-x86_64` and `nucleotide-remote-linux-aarch64`.
- Windows: run `.\scripts\setup-windows-runtime.cmd` (or the PowerShell equivalent) to stage `crates\nucleotide\runtime`; it fetches/builds grammars and excludes `gotmpl` by default. Windows packaging uses `.\scripts\package-velopack.ps1 -RequireRemoteHelpers` when the remote helpers are required.
- Helix runtime lookup may use `runtime/`, Cargo's pinned Helix checkout, or an explicit script source. Runtime staging is a packaging concern; do not make source changes depend on a locally generated runtime directory.

## Change hygiene

- Keep changes scoped and use Conventional Commit prefixes (`feat:`, `fix:`, `perf:`, `refactor:`, `chore:`, `docs:`, etc.).
- Run formatting plus the narrowest relevant tests; run locked workspace checks and layering when the dependency graph or cross-crate behavior changes.
- Prefer the manifests, CI, scripts, and representative source entrypoints over stale prose when documentation conflicts with executable configuration.
