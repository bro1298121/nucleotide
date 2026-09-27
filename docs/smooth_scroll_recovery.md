# Smooth scroll: recovery plan

Status: Phase 0 resolved. Phase 1 needs one reproduction run.
Working state: `.slim/deepwork/smooth-scroll-editor.md`. Trace logs: `.slim\trace/` (gitignored via `*.log`).

## Objective

Make the editor viewport scroll smoothly without ever stranding the Helix cursor
outside the visible area.

Three motions are in scope, all editor-viewport only:

| Motion | Status |
|---|---|
| Eased discrete jump (`page_down`/`page_up`, `scroll_down`/`scroll_up`) | **Done, user-verified** |
| Cursor-follow easing (`h`/`j`/`k`/`l`) | **Done, user-verified**, plus a one-way gesture carry |
| Mouse wheel | **Done, user-verified** — per-notch easing and a post-stop glide |

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

## Phase 1 — Measured reproduction (DONE)

Run the working configuration above, reproduce, and quit so the log flushes.

The log settled it. A `page_down` tween (0 -> 728px, 28 rows, 228ms) lived **2.85 ms**,
was re-anchored twice, and was cancelled. 5/5 reproductions identical:

```
.340805 scroll_animation:168 tween armed from=0px to=728px duration_ms=228.0
.341195 scroll_animation:206 tween sampled elapsed_ms=0.392 t=0.0017 position=2.501146px
.343636 scroll_animation:246 re-anchored from=2.501146px to=728px remaining_ms=225.1683
.343651 scroll_animation:246 re-anchored from=2.501146px to=728px remaining_ms=225.1502
.343658 scroll_animation:119 tween cancelled reason="helix_view_sync"
.343661 scroll_manager:243  view sync tween_active=true current_row=0 incoming_row=0 preserved_subrow=true resolved_y=2.501146px
```

Correction to my own narration: the "2.5px of 728px" figure is the **re-anchored origin**,
not eased travel. The tween was killed right after the first `retarget_from`, before it
eased anywhere. The `duration_ms=225.15` against a requested 228ms is the fingerprint of
`retarget_from` carrying the remaining budget forward.

### The causal story I got wrong

I first wrote that Helix was echoing a **stale** row. That is backwards. Helix's stored
row is never stale — the crossing-gated sync is precisely what keeps it correct, because
if the top row did not change this frame then the row Helix holds IS the current top row.
Row 0 is genuinely the top row. The tween's *destination* is simply unpublished by design.

A second correction I owed myself: the first instrumented run appeared to show "no tween
was ever armed". That was an artefact of my own `RUST_LOG` filter — the arming, cancel,
sampler and clamp traces all live in `scroll_animation.rs`, and I had only enabled the
`scroll_manager` and `viewport` modules. I had treated evidence I filtered out myself as
evidence of absence. The filter must name all three modules.

## Phase 2 — Fix the identified cause (DONE, user-verified)

`set_scroll_position_from_view_sync_preserving_subrow_offset` now hoists `preserved_subrow`
above the cancel and gates it on `!preserved_subrow`. Reason strings split into
`helix_view_sync_row_change` / `helix_view_sync_same_row`.

The invariant, local and decidable in one line: `preserved_subrow == true` implies the
resolved `y` **is** `current.y`, so the call provably changes nothing vertically, so it has
no justification for destroying the animation that would have moved it. The function was
self-contradictory.

An independent review rejected the `last_reported_view_row` echo discriminator I had
proposed, for two reasons worth preserving:

1. It is a **strict subset** of the trivial fix — its third conjunct
   `incoming_line == current_line` *is* `preserved_subrow`, so it cannot catch anything the
   simple fix lets through. Narrower, not safer.
2. It has a **cold-start hole that fails the repro**. On a fresh file the field stays `None`
   forever, because with the cursor at row 0 `has_pending_view_sync()` is false on every
   paint, so the push function is never called and no write site inside it can ever fire.

Not `page_down`-specific: a tween dies on the first frame in which it has not crossed a row,
and at these durations the first sample is always sub-row. A 1-row `scroll_down` is
20px/66ms and samples ~8.5px on frame 1. Every tween died on frame 1; `page_down` was just
where it was most visible. Regression tests must not be gated on `page_down`.

Blast radius: `scroll_manager.rs:214` has one production caller, which has two, which has one.

Verified: `cargo +stable test --release -p nucleotide-editor` and
`cargo +stable build --release -p nucleotide --bin nucl` both pass, and the user confirms
smooth `page_down` scrolling in the running app. This is the first fix in this bug's history
verified by felt experience rather than tests alone.

## Phase 3a — Wheel glide (IN PROGRESS)

User-selected feel model: **1:1 immediate response plus an extra eased glide after the wheel
stops.** Not the Firefox model — the view must never lag the wheel, because editor scrolling
is about precision. Vertical only; `h`/`l` stay 1:1.

The Phase 2 fix is a hard prerequisite: a glide is sub-row on its first frame exactly like a
fixed jump, so without the same-row cancel gate every glide would die on frame 1.

Highest-risk failure mode: `scroll_needs_frames()` must include a pending gesture so the
frame loop keeps ticking through the 90ms idle window, which means the gesture accumulator
**must** be cleared unconditionally when a gesture ends. If a below-threshold gesture left
the accumulator set, the frame loop would never terminate.

Constants: `RATIO 0.25 / MAX_PX 120 / GLIDE_MS 140 / ARM_MIN_PX 72 / ARM_MIN_EVENTS 2 /
GESTURE_IDLE_MS 90`. Gated behind its own config key, default true.

## Phase 3b — Cursor-follow easing (DONE, user-verified)

`h`/`j`/`k`/`l` are eased. The `Scrolloff` reveal arms a tween over
`editor_jump_duration(1) = 66ms`; it snaps instead when the travel exceeds
`visible_rows - margin`, because past that the cursor leaves the *painted* band and is not
drawn at all (`document_frame_painter.rs` returns `None` outside the rendered range).
`Top`/`Center`/`Bottom` and the `apply_scroll_request` route stay instant, pinned by tests.

Two prerequisites had to land first, and neither was optional:

1. **The band origin had to move to the destination row.** Measured, not predicted: a
   `page_down` tween put the destination at row 40 with the cursor at row 45, and a band
   built from the mid-tween row 5 judged it out of band and dragged the view to row 11.
2. **A tween armed during paint needs a frame.** The frame driver consults
   `scroll_needs_frames()` at the top of `render`, but a reveal is applied in the paint
   closure, which GPUI runs *after* `render` returns. The follow-up frame is scheduled with
   `cx.notify` from a `cx.defer` — and it must **not** also call `request_animation_frame`,
   which unwraps an empty `rendered_entity_stack` from a defer and panics
   (`gpui/src/window.rs:4315`). 398 green tests did not catch that one; only a real held
   `j` did.

## Phase 3c — Per-notch wheel easing (DONE, user-verified)

The 1:1 model was replaced after the user felt it: a Windows wheel delivers ~120px notches,
so 1:1 instant looked like a jump per notch. Each notch now retargets an 80ms tween onto
the gesture's accumulated destination. `scroll_by_delta` must report the **destination**,
because `surface.rs`'s `if !scroll_update.changed { return; }` would otherwise swallow the
update, and an armed tween with no frame never runs.

## Phase 3d — Key gesture carry (DONE, pending user verification)

A run of eased reveals is a gesture; on idle the view carries further along the travel
direction and **stops there**. Constants: `GESTURE_IDLE_MS 120 / MIN_GESTURE_ROWS 3 /
OVERSHOOT_RATIO 0.25 / OVERSHOOT_MAX 2 / OVERSHOOT_MS 100`.

**The carry is one-way, and that is the whole design.** It shipped with a settle leg twice
and both read as a rebound. The reason duration was never the variable: in the first attempt
(50ms out, 60ms back) the return's peak speed was already *lower* than the throw's and it
still read as a rebound. A reversal is noticed for existing, not for being fast.

The settle leg was believed necessary to stop the margin ratcheting. It is self-limiting —
the band is anchored to the top row against a fixed margin, so the resting distance is
`margin + min(rows * OVERSHOOT_RATIO, OVERSHOOT_MAX)`, bounded at 7 rows — and the user
accepted that steady state. `repeated_carries_hold_the_margin_instead_of_ratcheting` is the
guard for it.

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
