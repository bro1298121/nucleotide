# Smooth scroll: recovery plan

Status: Phase 0 resolved. Phase 1 needs one reproduction run.
Working state: `.slim/deepwork/smooth-scroll-editor.md`. Trace logs: `.slim\trace/` (gitignored via `*.log`).

## Objective

Make the editor viewport scroll smoothly without ever stranding the Helix cursor
outside the visible area.

Three motions are in scope, all editor-viewport only:

| Motion | Status |
|---|---|
| Eased discrete jump (`page_down`/`page_up`, `scroll_down`/`scroll_up`) | Animates; landing is wrong |
| Cursor-follow easing (`h`/`j`/`k`/`l`) | **Not implemented** — view jumps instantly |
| Wheel glide / inertia | **Not implemented** — wheel is 1:1 |

Correctness outranks feel: a cursor that is off-screen is a bug, not a tuning problem.

## Phase 0 — "Startup crash" (RESOLVED: there was no crash)

Reported symptom was `nucl.exe` dying immediately under the reproduction
environment variables, with an empty `stderr`. Root cause found by running the existing
release binary under bisected environment variables — no rebuild required:

`NUCLEOTIDE_LOG=trace` raises the **global** level, and the application already
emits `trace` on many hot paths. Measured throughput:

| Configuration | Result | Output in 15–20s |
|---|---|---|
| `NUCLEOTIDE_LOG=trace` | ran, UI unusable | **38,485,190 bytes to stdout** |
| all three vars | ran, UI unusable | 277,622,230 bytes to log file |
| `NUCLEOTIDE_LOG_DIR` only | ran normally | 18,091 bytes |
| `NUCLEOTIDE_LOG_NO_CONSOLE` only | ran normally | 0 bytes |

That is roughly **18 MB/s**. Startup never got to paint a window, which is
indistinguishable from a crash to a user. With the logging variables removed the app
started normally — matching the reported contrast exactly.

The process was never crashing. The Windows Application log contains no fault event
for `nucl.exe` (only unrelated .NET events whose request path contains "nucleotide"),
which is consistent with no fault at all.

### Working diagnostic configuration

`create_env_filter` (`crates/nucleotide-logging/src/layers.rs:115-129`) builds an
`EnvFilter` from the configured level, adds `module_levels` directives, and then —
if `RUST_LOG` is set — **discards the whole filter and uses `RUST_LOG` verbatim**.
That makes `RUST_LOG` the correct lever. `nucleotide_logging::trace` is a re-export of
`tracing::trace` (`lib.rs:14`), so the target is the module path.

```powershell
Remove-Item Env:\NUCLEOTIDE_LOG -ErrorAction SilentlyContinue
$env:RUST_LOG = "nucleotide_editor::scroll_manager=trace,nucleotide_editor::viewport=trace,warn"
$env:NUCLEOTIDE_LOG_DIR = "D:\helixperience\nucleotide\.slim\trace"
$env:NUCLEOTIDE_LOG_NO_CONSOLE = "1"
.\target\release\nucl.exe
```

Measured: **452,304 bytes over 20 s** and the application ran the full duration —
about 600x less output than the global-trace configuration.

### Incidental defect found (NOT bundled with scroll work)

`FileConfig` declares `max_size_mb: 50, max_files: 5`, but the observed log reached
**277,622,230 bytes** in a single file. Log rotation is not enforcing the configured
size cap. This is an independent bug in `nucleotide-logging`; it is recorded rather
than fixed here, because bundling it would make any scroll regression impossible to
bisect. It also means any future high-volume logging run can silently consume
gigabytes.

Also noted, currently believed benign: `ERROR gpui_windows::platform:280:
拒绝访问。 (0x80070005)` appears on startup in a run that does not crash.

## Why we are measuring instead of fixing

Two fixes landed, each verified present in the built binary, each correct about the code
it touched, and the reported symptom did not change at all:

1. **Redundant reveal clear** (`crates/nucleotide/src/workspace/mod.rs`, page-scroll
   path only). Removed a cursor reveal that was cancelling the tween on the first
   painted frame.
2. **Horizontal view sync no longer cancels** (`scroll_manager.rs`,
   `set_horizontal_scroll_offset_from_view_sync`). The Helix horizontal sync destroyed
   every tween while being mathematically incapable of moving the viewport
   vertically — it passed `current.y` through unchanged into a setter whose first
   statement is an unconditional `animation.cancel()`.

A third speculative fix would very likely also be locally correct and still wrong about
the cause. So the engine is now instrumented: every tween kill is logged with a
`reason` naming its cause, and only when it actually killed a live tween.

## Phase 1 — Measured reproduction (current)

Run the working configuration above, reproduce, and quit so the log flushes.

Reproduction: open `Cargo.lock`, jump to the top, one `page_down`, let it settle, then
repeated `j` until the cursor leaves the viewport.

Read the log and answer: which `reason` kills the tween; whether a tween is ever armed
for the `j` presses; whether `clamp_target_no_room` fires; whether `reveal_visual_row`
computes its band against a mid-tween row (`old_top_visual_row` vs
`tween_destination_row`).

Deliverable: a named killer with a call path, evidenced by the log rather than inferred.

## Phase 2 — Fix the identified cause

Scoped to whatever Phase 1 proves. Explicitly **not** bundled: the `last_reported_row`
echo discriminator, the `reveal_visual_row` band-origin fix, or the
double-`set_viewport_size` cleanup. Bundling makes a regression impossible to bisect,
and two of the three already failed to be the cause when bundled with something else.

## Phase 3 — Cursor-follow easing and wheel glide

Cursor-follow easing, then wheel glide. Cursor-follow is the higher-risk of the two: the
reveal path fights the tween by design today, and if the discriminator is wrong the
viewport fights its own cursor, which is worse than no animation. Safe fallback if it
proves unstable: keep the discrete tween and ease only the short case.

## Phase 4 — Cleanup and validation

Decide whether the `trace!` instrumentation stays. The `reason` plumbing is cheap and
genuinely useful for diagnosing this class of bug again, but it is diagnostic scaffolding
and should be an explicit choice, not an accident. Then address the deferred
`set_viewport_size`-twice-per-paint finding, which defeats the extent-setter equality
guard and re-anchors the tween twice per frame (wasteful, not currently incorrect —
heights are bit-identical, so vertical behaviour is unaffected).

Validation: `cargo test --release -p nucleotide-editor`,
`cargo build --release -p nucleotide --bin nucl`, plus the repo's own gates —
`./scripts/check-layering.sh`, `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- --deny warnings`.

## Tuning reference

Sluggishness is dead time, not total time. Judging by the number in the source tunes this
backwards: the old 224ms quint was perceptually ~98ms of motion plus 126ms of nothing,
while 280ms quad is perceptually ~252ms of motion.

Discrete: `MIN 60 / PER_ROW 6 / MAX 280` ms. Cursor-follow: `SNAP_ROWS 10 / MIN 60 /
PER_ROW 4 / MAX 100` ms. Wheel: `RATIO 0.25 / MAX_PX 120 / GLIDE_MS 140 /
ARM_MIN_PX 72 / GESTURE_IDLE_MS 90`.

If discrete feels too fast raise `MAX` to 320; too loose drop it to 240. If a held
`page_down` crawls, the cause is repeats **retargeting** rather than restarting from a
stale origin — no duration value fixes a restart.

## Static review already performed (do not repeat)

- `scroll_animation.rs` — no `RefCell` double-borrow. `cancel()` takes via
  `borrow_mut().take()` with the `RefMut` dropped at statement end; `animate_to_at`
  clones prior state out before its `borrow_mut()` write; `advance` clones before
  calling `cancel`. All borrows statement-scoped.
- `scroll_manager.rs` view-sync setter — hoisted reads touch only `Cell`s and
  `is_active()`; `tween_active` is materialised as a `bool` before `cancel()`'s
  `borrow_mut()`. No live borrow across it.
- `destination_position()` returns owned data; `pixels_to_anchor()` is float division
  whose `as usize` cast saturates, so a zero line height cannot panic.
- `nucleotide-logging` — every `unwrap`/`expect` is inside `#[cfg(test)]`; the logger
  has no panic path.
- `&'static str` satisfies `tracing::Value` (`tracing-core-0.1.34/src/field.rs:562`
  `impl Value for str`, plus the blanket `impl<'a, T: ?Sized> Value for &'a T`), so the
  bare `reason,` field compiles.
