// ABOUTME: Native pixel scroll state for the GPUI editor viewport
// ABOUTME: Tracks visual-row positions and sub-row offsets for smooth scrolling

use gpui::{Pixels, Point, Size, point, px, size};
use nucleotide_logging::trace;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use crate::scroll_animation::ScrollAnimation;

/// Mouse-wheel feel, gathered into one block because these are the knobs
/// to reach for first when the wheel feels wrong.
///
/// The model is the Firefox/Chrome one, and it is deliberately *not* 1:1:
///
///  - **during** the gesture every vertical notch retargets a short eased tween
///    toward the position the whole gesture has asked for so far, so the view
///    glides continuously instead of stepping. A Windows wheel delivers discrete
///    notches of roughly 120px, so the old 1:1-and-instant path produced exactly
///    the visible jump per notch that this removes,
///  - **after** the wheel goes quiet, a further eased glide proportional to what
///    the gesture accumulated is tweened on top of the position the gesture
///    reached. Total travel is therefore about `1 + RATIO` times the raw wheel
///    travel, which is accepted: the glide is a fling tail, not a correction.
///
/// Each notch gets a *fresh* full duration for its retarget, because a notch is
/// a new intent that deserves its own flight time. That is the one place a fresh
/// deadline is correct; every other retarget in this file carries the remaining
/// budget forward (see [`ScrollAnimation::retarget_from`]).
///
/// Horizontal wheel travel is untouched: 1:1, instant, never eased, never glided.
/// `h`/`l` are 1:1 by design for the same reason — they are deliberate, repeated
/// keypresses and already arrive as many small steps.
///
/// Only `ease_out_quad` is used, so no new curve is introduced here.
mod wheel_glide {
    use gpui::{Pixels, px};
    use std::time::Duration;

    /// How long the in-gesture tween onto the accumulated wheel target runs.
    ///
    /// A distinct constant from `GLIDE_MS` because the two are different motions:
    /// this is one notch settling onto a position the user has already asked for,
    /// whereas the glide is the long tail of a finished flick.
    ///
    /// Deliberately shorter than both other wheel timings:
    ///  - shorter than [`GESTURE_IDLE_MS`] so a single deliberate notch has
    ///    settled *before* the idle window elapses. The post-stop glide then
    ///    continues an already-arrived view instead of correcting one that is
    ///    still in the air, and it never has to replace a live tween.
    ///  - shorter than [`GLIDE_MS`], because a notch is a small movement that
    ///    should arrive quickly.
    ///
    /// Notches arriving faster than this simply retarget a tween that is still in
    /// flight, which is the continuous-scroll case and is the entire point.
    pub(crate) const TWEEN_MS: u64 = 80;
    /// Fraction of the gesture's accumulated vertical travel added as glide.
    /// `0.25` means a 400px gesture glides a further 100px.
    pub(crate) const RATIO: f32 = 0.25;
    /// Per-glide cap in pixels, applied *after* `RATIO`, in both directions.
    /// A hard wheel fling therefore cannot fling the viewport arbitrarily far.
    pub(crate) const MAX_PX: Pixels = px(120.0);
    /// How long the glide tween runs.
    pub(crate) const GLIDE_MS: u64 = 140;
    /// Accumulated travel a gesture needs before it is allowed to arm a glide.
    pub(crate) const ARM_MIN_PX: f32 = 72.0;
    /// Wheel events a gesture needs before it is allowed to arm a glide. One
    /// deliberate notch is a precision movement, not a flick, and must not
    /// glide: it would move the viewport further than the user asked for.
    pub(crate) const ARM_MIN_EVENTS: u32 = 2;
    /// Wheel silence that ends a gesture.
    pub(crate) const GESTURE_IDLE_MS: u64 = 90;

    pub(crate) const TWEEN_DURATION: Duration = Duration::from_millis(TWEEN_MS);
    pub(crate) const GLIDE_DURATION: Duration = Duration::from_millis(GLIDE_MS);
    pub(crate) const GESTURE_IDLE: Duration = Duration::from_millis(GESTURE_IDLE_MS);
}

/// Keyboard "inertia" for the cursor-follow scroll, gathered into one block
/// because these are the knobs to reach for first when the follow feels dead or
/// too loose.
///
/// This is deliberately **not** the wheel's fling tail. A wheel emits a
/// continuous delta stream, so when the hand stops there is a known distance
/// left over to glide. A keypress moves the cursor exactly one row and the view
/// has already eased onto it, so there is no leftover to glide — the only honest
/// analogue of momentum is the *gesture*:
///
///  - a single deliberate `j`/`k` is a precision movement. It is one reveal, one
///    row, and it must not overshoot at all. [`MIN_GESTURE_ROWS`] is what makes
///    that true, and it is a row count rather than a pixel count precisely so a
///    one-row press cannot reach it on any window size or line height,
///  - reveals that arrive within [`GESTURE_IDLE_MS`] of each other are one
///    gesture. When the keys go quiet the view eases a few rows *further along*
///    the direction it was already travelling ([`OVERSHOOT_MS`]) and stops there.
///    The carry scales with how far the gesture travelled ([`OVERSHOOT_RATIO`])
///    and is capped ([`OVERSHOOT_MAX`]), so a long `j` hold cannot fling the
///    viewport.
///
/// # One leg, and never a reversal
///
/// The carry is one-way: out, decelerating, done. That is structurally the wheel
/// glide, and it is why this reads as inertia where a return leg did not — a
/// fling that keeps going and stops, rather than one that goes and comes back.
///
/// A settle leg was here and was removed, and the reason is **not** the duration
/// of either leg: any reversal in the direction of travel reads as a rebound, and
/// duration was never the variable. Two pairs were shipped and measured — 50ms
/// out against 60ms back, then 100ms out against 40ms back — and both read as a
/// rebound, including the pair whose return was *shorter* than its throw. A
/// reversal is noticed for existing, not for being fast, and the faster return
/// made it more obvious rather than less.
///
/// So there is no settle constant to tune and no second phase to wait on. If you
/// find yourself wanting one back, the thing to check first is not the timing: it
/// is whether the carry is being derived from a stale row. See
/// [`ScrollManager::arm_scrolloff_overshoot`], which is also where the derivation
/// that stops the carry from ratcheting the `scrolloff` margin outward is spelled
/// out.
///
/// The safety argument for the whole feature is one line, and it is the reason
/// the carry is allowed to exist at all: it always moves the view *further along*
/// the travel direction, which is the one direction in which the cursor retreats
/// into the viewport rather than toward an edge.
///
/// Only *eased* `Scrolloff` reveals feed this. A reveal that snaps is a jump
/// rather than a glide and resets the gesture instead of joining it.
/// `Top`/`Center`/`Bottom`, the `apply_scroll_request` route and horizontal
/// travel never reach it, and with the tween engine off the whole feature is
/// inert.
///
/// The module is `pub(crate)` rather than private so the viewport tests can name
/// the durations directly instead of hardcoding them: retuning the feel should
/// not invalidate tests that are supposed to be pinning the mechanism. The
/// durations are the tunable; the mechanism is not.
pub(crate) mod key_inertia {
    use gpui::{Pixels, px};
    use std::time::Duration;

    /// Key silence that ends a gesture.
    ///
    /// Longer than a one-row reveal's own `editor_jump_duration(1) = 66ms`, so
    /// the last reveal of an ordinary gesture has *landed* on the exact margin
    /// before the carry is decided. It is not longer than the longest eased
    /// reveal — a 35-row reveal runs 270ms — which is exactly why the carry is
    /// measured from the margin rather than from wherever the live position
    /// happens to be when the window elapses.
    pub(crate) const GESTURE_IDLE_MS: u64 = 120;
    /// A gesture has to travel at least this many rows before it may overshoot.
    ///
    /// Below this, no carry at all. One deliberate `j` is one row, so it can
    /// never qualify: precision on a single step outranks any feel effect.
    pub(crate) const MIN_GESTURE_ROWS: f32 = 3.0;
    /// Carry rows = gesture rows * this.
    pub(crate) const OVERSHOOT_RATIO: f32 = 0.25;
    /// Cap on the carry, in rows, applied *after* `OVERSHOOT_RATIO`.
    ///
    /// Reached at `2 / 0.25 = 8` rows, so an 8-row gesture carries the maximum
    /// and a 12-row one carries the same 2 rows rather than 3. This cap is also
    /// what bounds how far the carry can move the cursor *inside* the band, and
    /// therefore what stops the `scrolloff` margin ratcheting outward gesture
    /// after gesture; see [`ScrollManager::arm_scrolloff_overshoot`].
    pub(crate) const OVERSHOOT_MAX: f32 = 2.0;
    /// How long the carry runs. The only leg, so there is nothing to compare it
    /// against — and nothing to compare it *to*, deliberately; see the module
    /// doc for the measurement behind that.
    ///
    /// It has to be long enough that the deceleration reads as the tail of the
    /// gesture rather than as a jump at the end of it, and that is its whole
    /// job.
    pub(crate) const OVERSHOOT_MS: u64 = 100;
    /// How far the carry's landing may be from its recorded target and still
    /// count as having landed.
    ///
    /// The engine returns `AnimState::to` verbatim at `t >= 1`, so a landing is
    /// exact; the tolerance only keeps a future change to the sampler from
    /// turning "landed" into a coin flip. It is compared against *positions*,
    /// never against rows, so it cannot mask a whole-row error.
    ///
    /// With a single leg this decides nothing about the state any more — a
    /// landed carry and an interrupted one both end the feature — so it is read
    /// only to tell those two apart in the trace.
    pub(crate) const LANDED_EPSILON: Pixels = px(0.01);

    pub(crate) const GESTURE_IDLE: Duration = Duration::from_millis(GESTURE_IDLE_MS);
    pub(crate) const OVERSHOOT_DURATION: Duration = Duration::from_millis(OVERSHOOT_MS);
}

/// The vertical wheel gesture currently in progress, or "none".
///
/// Held in a `RefCell` because the wheel event that starts a gesture arrives
/// on a `&self` path (`scroll_by_delta`) and must not require `&mut self`.
#[derive(Debug, Clone, Default)]
struct WheelGesture {
    /// Signed accumulated vertical travel, in the same sign convention as the
    /// incoming `delta.y` (negative is a downward wheel). Held as a plain
    /// `f32` because it is only ever summed, compared and scaled — never
    /// stored as the scroll position.
    accumulated: f32,
    /// How many wheel events are folded into `accumulated`.
    events: u32,
    /// Timestamp of the most recent wheel event. `None` means no gesture is
    /// pending, which is what keeps the frame loop from spinning forever.
    last_event: Option<Instant>,
    /// The vertical position the gesture as a whole has now asked for, or
    /// `None` when no gesture is pending.
    ///
    /// In `scroll_position().y` space, where a **larger** value means further
    /// **down** — the opposite of the incoming `delta.y`. A notch is added here
    /// rather than to the live position, which is what makes N notches of X land
    /// on N*X from where the gesture started regardless of how far the view had
    /// already travelled.
    ///
    /// Only ever written by the eased path, and reset to `None` by
    /// [`Self::clear_wheel_gesture`]. That is what bounds it: a gesture's target
    /// cannot outlive its own idle window, so a later gesture always starts from
    /// the live position rather than from a stale one, and a switch flipped on
    /// mid-gesture cannot apply a target accumulated while nothing was animating.
    target: Option<Pixels>,
}

/// Which part of the cursor-follow inertia, if any, is in progress.
///
/// Exactly one phase is "quiet": `Idle`. Every other phase gives
/// [`ScrollManager::scroll_needs_frames`] a reason to ask for another frame, so
/// every other phase carries a deadline that the driver is guaranteed to reach
/// — either the gesture's idle window, or a tween the engine ends on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyInertiaPhase {
    /// Nothing collected and no carry in flight. The only phase that asks for no
    /// further frames.
    Idle,
    /// Eased reveals are being folded into a gesture and `last_reveal` is the
    /// deadline that ends it.
    Collecting,
    /// The carry is flying towards `outward_target`.
    Outward,
}

/// The cursor-follow inertia in progress, or "none".
///
/// Deliberately the same shape as [`WheelGesture`] — an accumulator, a timestamp
/// that bounds it, and a `None`/`Idle` that is what stops the frame loop — for
/// the same reasons, and with the same single leg the wheel glide has. The one
/// difference is that the accumulator is in **visual rows** rather than pixels:
/// rows are the unit the feature is specified in, and both thresholds are row
/// counts, so a line-height change mid-gesture cannot corrupt the arithmetic the
/// way a pixel accumulator's thresholds could.
#[derive(Debug, Clone, Copy)]
struct KeyInertia {
    phase: KeyInertiaPhase,
    /// Signed travel the gesture has accumulated, in visual rows, in the same
    /// sign convention as a reveal's own `travel_rows`: **positive means the
    /// gesture is travelling down the document**, i.e. towards a larger
    /// `scroll_position().y`. Never a scroll position and never a pixel
    /// distance, so a line-height change mid-gesture cannot corrupt it.
    rows: isize,
    /// Timestamp of the most recent eased reveal. `None` when no gesture is
    /// collecting, which is what bounds it and what keeps
    /// [`ScrollManager::scroll_needs_frames`] from being held open.
    last_reveal: Option<Instant>,
    /// The exact `scrolloff`-margin row the gesture's last reveal asked for.
    /// The carry is measured from that row, not from the live position.
    ///
    /// This field is written only while `Collecting` and read only by
    /// [`ScrollManager::step_key_inertia`] on the way to arming the carry; the
    /// `Outward` phase has no use for it. That is the one place a field like
    /// this is not speculative — it is the handoff between "a reveal told us the
    /// margin" and "the window closed, so act on it", and the live position
    /// cannot serve in its place, because a long eased reveal is still short of
    /// the margin when the window elapses.
    settle_row: usize,
    /// Where the carry is actually flying, read back from the engine *after*
    /// arming. A carry clipped by the top or bottom of the document lands on the
    /// clamped value, not on the requested one, and "did it land" has to be asked
    /// about the value it will really reach.
    outward_target: Pixels,
}

impl Default for KeyInertia {
    fn default() -> Self {
        Self {
            phase: KeyInertiaPhase::Idle,
            rows: 0,
            last_reveal: None,
            settle_row: 0,
            outward_target: px(0.0),
        }
    }
}

/// Manages native scroll state for a document viewport.
#[derive(Clone, Debug)]
pub struct ScrollManager {
    /// Unique ID for debugging
    id: usize,
    /// Cached line height in pixels
    line_height: Rc<Cell<Pixels>>,
    /// Total number of lines in the document
    total_lines: Rc<Cell<usize>>,
    /// Content width in pixels for horizontal scrolling
    content_width: Rc<Cell<Pixels>>,
    /// Current scroll position in pixels (positive when scrolled down/right)
    scroll_position: Rc<Cell<Point<Pixels>>>,
    /// Viewport size in pixels
    viewport_size: Rc<Cell<Size<Pixels>>>,
    /// Track if native viewport scroll changed and needs sync to Helix
    pending_view_sync: Rc<Cell<bool>>,
    animation: ScrollAnimation,
    /// Accumulated state of the wheel gesture in progress.
    wheel_gesture: Rc<RefCell<WheelGesture>>,
    /// Whether the post-gesture glide is allowed at all. When false no
    /// gesture is recorded at all, so the whole feature is inert.
    wheel_glide_enabled: Rc<Cell<bool>>,
    /// The cursor-follow inertia in progress.
    key_inertia: Rc<RefCell<KeyInertia>>,
}

impl ScrollManager {
    pub fn new(line_height: Pixels) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

        Self {
            id: NEXT_ID.fetch_add(1, Ordering::SeqCst),
            line_height: Rc::new(Cell::new(line_height)),
            total_lines: Rc::new(Cell::new(1)),
            content_width: Rc::new(Cell::new(px(0.0))),
            scroll_position: Rc::new(Cell::new(point(px(0.0), px(0.0)))),
            viewport_size: Rc::new(Cell::new(size(px(800.0), px(600.0)))),
            pending_view_sync: Rc::new(Cell::new(false)),
            animation: ScrollAnimation::new(),
            wheel_gesture: Rc::new(RefCell::new(WheelGesture::default())),
            // Defaults to on, matching `EditorScrollConfig`'s shipped default.
            // The tween engine gate (`smooth_scrolling`) is separate: a glide
            // cannot animate unless the engine is enabled either.
            wheel_glide_enabled: Rc::new(Cell::new(true)),
            // No separate switch. The feature *is* a tween, so `smooth_scrolling`
            // is the whole gate and gating it anywhere else could only let the
            // two disagree.
            key_inertia: Rc::new(RefCell::new(KeyInertia::default())),
        }
    }

    pub(crate) fn line_height(&self) -> Pixels {
        self.line_height.get()
    }

    pub(crate) fn total_lines(&self) -> usize {
        self.total_lines.get()
    }

    pub(crate) fn viewport_size(&self) -> Size<Pixels> {
        self.viewport_size.get()
    }

    pub(crate) fn has_pending_view_sync(&self) -> bool {
        self.pending_view_sync.get()
    }

    pub(crate) fn clear_pending_view_sync(&self) {
        self.pending_view_sync.set(false);
    }

    /// Update the total number of lines in the document
    ///
    /// These four extent setters all run once per painted frame. When the value
    /// is unchanged the clamp is an identity and the re-anchor is a no-op, so
    /// bailing out early is what keeps a running scroll tween's clock intact.
    pub(crate) fn set_total_lines(&mut self, total_lines: usize) {
        if self.total_lines.get() == total_lines {
            return;
        }
        self.total_lines.set(total_lines);
        self.clamp_scroll_position_after_extent_change();
    }

    /// Update the horizontal content width in pixels.
    pub(crate) fn set_content_width(&mut self, content_width: Pixels) {
        let clamped = content_width.max(px(0.0));
        if self.content_width.get() == clamped {
            return;
        }
        self.content_width.set(clamped);
        self.clamp_scroll_position_after_extent_change();
    }

    /// Update the viewport size
    pub(crate) fn set_viewport_size(&mut self, size: Size<Pixels>) {
        if self.viewport_size.get() == size {
            return;
        }
        self.viewport_size.set(size);
        self.clamp_scroll_position_after_extent_change();
    }

    /// Update the line height
    pub(crate) fn set_line_height(&mut self, line_height: Pixels) {
        if self.line_height.get() == line_height {
            return;
        }
        let previous_line_height = self.line_height.get();
        let previous_position = self.scroll_position.get();
        let previous_top_line = self.pixels_to_anchor(previous_position.y);
        let previous_offset = if previous_line_height > px(0.0) {
            (previous_position.y - previous_line_height * previous_top_line as f32).max(px(0.0))
        } else {
            px(0.0)
        };

        self.line_height.set(line_height);

        if previous_line_height > px(0.0) && line_height > px(0.0) {
            let offset_fraction = (previous_offset / previous_line_height).clamp(0.0, 1.0);
            let scaled_position = point(
                previous_position.x,
                self.anchor_to_pixels(previous_top_line) + (line_height * offset_fraction),
            );
            self.set_scroll_position_internal(scaled_position, false, "line_height_change");
            if self.pixels_to_anchor(self.scroll_position.get().y) != previous_top_line {
                self.pending_view_sync.set(true);
            }
        } else {
            self.clamp_scroll_position_after_extent_change();
        }

        self.retarget_scroll_animation();
    }

    /// Get the maximum scroll offset in pixels
    pub fn max_scroll_offset(&self) -> Size<Pixels> {
        let total_lines = self.total_lines.get();
        let line_height = self.line_height.get();
        let content_height = line_height * (total_lines as f32);
        let viewport_height = self.viewport_size.get().height;
        let max_y = (content_height - viewport_height).max(px(0.0));
        let content_width = self.content_width.get();
        let viewport_width = self.viewport_size.get().width;
        let max_x = (content_width - viewport_width).max(px(0.0));

        size(max_x, max_y)
    }

    /// Get the current scroll position in pixels (positive when scrolled down/right)
    pub fn scroll_position(&self) -> Point<Pixels> {
        self.scroll_position.get()
    }

    /// Get the scroll offset for GPUI scrollable (negative when scrolled down/right)
    pub fn scroll_offset(&self) -> Point<Pixels> {
        let pos = self.scroll_position.get();
        point(-pos.x, -pos.y)
    }

    /// Set the scroll position in pixels from native interaction (positive when scrolled down/right)
    /// This marks the position as needing sync back to Helix
    ///
    /// `reason` names the cause of the write, so a tween this write kills is
    /// attributable to the interaction that killed it.
    pub(crate) fn set_scroll_position(&self, position: Point<Pixels>, reason: &'static str) {
        self.set_scroll_position_internal(position, true, reason);
    }

    /// Set the scroll offset in pixels from native interaction (negative when scrolled down/right)
    /// This marks the position as needing sync back to Helix
    pub(crate) fn set_scroll_offset(&self, offset: Point<Pixels>, reason: &'static str) {
        self.set_scroll_offset_internal(offset, true, reason);
    }

    fn set_scroll_offset_internal(
        &self,
        offset: Point<Pixels>,
        from_native_view: bool,
        reason: &'static str,
    ) {
        let position = point(-offset.x, -offset.y);
        self.set_scroll_position_internal(position, from_native_view, reason);
    }

    /// Apply a GPUI-style pixel scroll delta to the current offset.
    ///
    /// Returns whether the request moves the viewport, and how many whole
    /// document lines it crosses. Both are reported against the **destination**,
    /// not the live position, for the same reason [`Self::scroll_by_visual_rows`]
    /// reports the destination: with a tween in flight nothing has moved yet, and
    /// a caller told "nothing changed" would skip its repaint and its Helix sync,
    /// leaving an armed tween with no frame to run on.
    ///
    /// Horizontal travel is applied 1:1 and instantly. Vertical travel is
    /// accumulated into [`WheelGesture::target`] and eased onto, each notch
    /// retargeting a fresh tween; a gesture that goes idle then adds its glide on
    /// top. See [`Self::wheel_target_for_delta`].
    pub(crate) fn scroll_by_delta(&self, delta: Point<Pixels>) -> (bool, isize) {
        self.scroll_by_delta_at(delta, Instant::now())
    }

    /// [`Self::scroll_by_delta`] with an injected tween clock.
    ///
    /// `now` is a parameter rather than a clock read inside so that the whole
    /// wheel sequence — per-notch retargeting, a retarget arriving mid-flight,
    /// the idle window elapsing, the glide arming and its flight — is reproducible
    /// from an injected `Instant` in a test. Each notch's deadline is exactly
    /// `now + TWEEN_MS`, so sampling at `now + k` measures `k` of easing with no
    /// dependence on how long the arming call itself happened to take.
    pub(crate) fn scroll_by_delta_at(&self, delta: Point<Pixels>, now: Instant) -> (bool, isize) {
        // A wheel gesture is a continuous user intent; it overrides an in-flight
        // discrete tween rather than fighting it. The same cancel kills an
        // in-flight glide, so a new wheel event resumes from wherever the glide
        // had actually reached, rather than snapping to its target first.
        self.animation.cancel("wheel_scroll");
        self.record_wheel_gesture(delta.y);

        let old_position = self.scroll_position.get();
        let old_line = self.pixels_to_anchor(old_position.y);

        // Horizontal wheel travel is 1:1 and instant by design and is never part
        // of the vertical gesture, so it is written straight through whatever the
        // vertical axis is about to do. `delta` is an offset delta and a position
        // is its negation (see [`Self::scroll_offset`]), so the move is `-delta.x`.
        if delta.x != px(0.0) {
            self.set_scroll_position_internal(
                point(old_position.x - delta.x, old_position.y),
                false,
                "wheel_scroll",
            );
        }

        // A vertical notch is eased onto only when there is a tween to ease it
        // with. With either switch off this falls through to the original 1:1
        // instant write, so a disabled feature is exactly the old behaviour.
        let eased = delta.y != px(0.0) && self.wheel_tween_enabled();
        if eased {
            // The deadline is set here and only here. `animate_scroll_to_at`
            // replaces the tween state outright, so this stores `now` and the
            // full `TWEEN_DURATION`: a fresh, full-length flight for this
            // notch's own intent. It deliberately does not go through
            // `retarget_scroll_animation`, which is the extent-change
            // re-anchor — that one carries the remaining budget forward, and
            // using it here would cap the tween at its one-frame floor instead
            // of easing at all. No per-frame caller re-anchors either, so
            // nothing can reset this clock under the tween's feet.
            let target = self.wheel_target_for_delta(delta.y);
            let current = self.scroll_position.get();
            self.animate_scroll_to_at(
                point(current.x, target),
                wheel_glide::TWEEN_DURATION,
                now,
            );
        } else if delta.y != px(0.0) {
            let current = self.scroll_position.get();
            self.set_scroll_position_internal(
                point(current.x, current.y - delta.y),
                false,
                "wheel_scroll",
            );
        }

        // Report the destination, not the live position: see the doc comment.
        let new_position = self.destination_position();
        let new_line = self.pixels_to_anchor(new_position.y);
        let crossed_lines = new_line as isize - old_line as isize;

        // `pending_view_sync` is the paint-time channel that pushes the **live**
        // top row into Helix, so it is armed only for motion that has already
        // happened. When a tween is armed, the whole-row crossings that actually
        // reach Helix are armed inside `advance_scroll_animation_at`, one per
        // painted sample; arming here too would report a destination the view has
        // not arrived at. The horizontal move is a real instantaneous write in
        // both paths, so it arms either way.
        if old_position.x != new_position.x || (!eased && crossed_lines != 0) {
            self.pending_view_sync.set(true);
        }

        (old_position != new_position, crossed_lines)
    }

    /// Whether a vertical wheel notch may be eased onto rather than applied 1:1.
    ///
    /// Two independent switches, and both have to be on:
    ///  - `wheel_glide`, the user-facing master switch for wheel smoothing, and
    ///  - the tween engine, because a smoothed notch *is* a tween and there is
    ///    nothing to ease it with otherwise.
    ///
    /// The engine is tested here rather than left to `animate_to_at` refusing.
    /// That refusal is a *cancel*, so letting it fire would silently turn "no
    /// smoothing" into "no wheel", and the caller could not tell them apart.
    fn wheel_tween_enabled(&self) -> bool {
        self.wheel_glide_enabled.get() && self.scroll_animation_enabled()
    }

    /// The vertical position this gesture has now asked for, in position space.
    ///
    /// Sign spaces. The incoming `delta.y` is GPUI's: NEGATIVE means scrolling
    /// DOWN. `scroll_position().y` is the opposite: a LARGER position means
    /// further down. Converting a notch into position space therefore means
    /// negating it, and the arithmetic here is `previous_target - delta.y`.
    /// Adding them directly — or dropping the negation — sends a downward notch
    /// *upward*, which no distance check can see and every user can.
    ///
    /// The notch is added to the **previous target**, not to the live position.
    /// That is what makes N notches of X land on N*X from where the gesture
    /// started, and it is the difference between extending a flight and
    /// discarding it: adding to the live position instead would throw away
    /// whatever the in-flight tween had not covered yet, so a fast wheel would
    /// travel less than the user asked for and a slow one would land short. A
    /// retarget therefore eases from the live position onto an accumulated
    /// destination, never from a stale origin.
    ///
    /// Clamped into the scrollable range on every step, so a gesture held against
    /// the end of the document cannot accumulate a destination the document can
    /// never reach.
    fn wheel_target_for_delta(&self, delta_y: Pixels) -> Pixels {
        let start = self
            .wheel_gesture
            .borrow()
            .target
            .unwrap_or_else(|| self.scroll_position.get().y);
        // `clamp_position` is the manager's single clamping rule, so its y leg is
        // taken from it rather than restated here. The x leg is discarded.
        let clamped = self.clamp_position(point(px(0.0), start - delta_y)).y;
        self.wheel_gesture.borrow_mut().target = Some(clamped);
        clamped
    }

    /// Fold one wheel event's vertical travel into the pending gesture.
    ///
    /// Only `delta.y` is accumulated. Horizontal wheel travel is 1:1 by design
    /// and never glides, so it is not tracked, and an event with no vertical
    /// component is not a vertical gesture event at all: it neither extends the
    /// idle window nor contributes travel.
    fn record_wheel_gesture(&self, delta_y: Pixels) {
        if !self.wheel_glide_enabled.get() {
            return;
        }

        let delta_y: f32 = delta_y.into();
        if delta_y == 0.0 {
            return;
        }

        let now = Instant::now();
        let mut gesture = self.wheel_gesture.borrow_mut();

        // A direction flip restarts the accumulation rather than netting against
        // it. A down-then-up flick that nets to nearly zero would otherwise
        // produce either no glide or a glide in a direction the user did not
        // end the gesture on.
        if gesture.events > 0 && (delta_y > 0.0) != (gesture.accumulated > 0.0) {
            gesture.accumulated = 0.0;
            gesture.events = 0;
        }

        gesture.accumulated += delta_y;
        gesture.events += 1;
        gesture.last_event = Some(now);
    }

    /// Ease onto a `Scrolloff` reveal's target and, if the reveal really moved,
    /// fold it into the pending cursor-follow gesture.
    ///
    /// Arming and recording are one call on purpose. Three outcomes have to be
    /// told apart, and all three can be told apart only *after* the arming:
    ///
    ///  - moved: a real step, so it extends the gesture,
    ///  - a zero-travel reveal — the cursor was already sitting on the margin.
    ///    It is not a step at all, so it neither extends the idle window nor
    ///    resets it, the same exclusion the wheel applies to an event with no
    ///    vertical component,
    ///  - a target that clamped onto the row the view is already on. That
    ///    *cancels* rather than arms, nothing moved, so the gesture resets —
    ///    and the cancel also killed whatever carry was in flight, which has to
    ///    be cleared here or the driver would sit in `Outward` waiting for a
    ///    tween that no longer exists.
    ///
    /// Deciding it here keeps the reveal in `viewport.rs` a single call on a
    /// paint path that has to stay trivial.
    pub(crate) fn ease_scrolloff_reveal_to(
        &self,
        target: Point<Pixels>,
        travel_rows: isize,
        settle_row: usize,
        duration: Duration,
    ) {
        self.animate_scroll_to(target, duration);
        if travel_rows != 0 && !self.scroll_animation_active() {
            self.clear_scrolloff_inertia();
        } else {
            self.record_scrolloff_reveal(travel_rows, settle_row);
        }
    }

    /// Fold one **eased** `Scrolloff` reveal into the pending key gesture.
    ///
    /// `travel_rows` is the reveal's own signed travel in visual rows and
    /// `settle_row` the exact row it eases onto. Only the eased path calls this;
    /// a reveal that snapped reaches [`Self::clear_scrolloff_inertia`] instead.
    ///
    /// A direction flip restarts the accumulation rather than netting against
    /// it, for [`Self::record_wheel_gesture`]'s reason: a down-then-up gesture
    /// that nets to nearly nothing would either not carry at all or carry in a
    /// direction the user did not end on.
    ///
    /// Setting the phase to `Collecting` unconditionally is also how a reveal
    /// that arrives while a carry is in flight drops that carry. The reveal arms
    /// its own tween and the engine replaces whatever was flying, so the carry
    /// can no longer reach its recorded target — and the new margin is the one
    /// the next carry is measured from, which is exactly why the phase is put
    /// back to `Collecting` rather than left at `Outward`.
    pub(crate) fn record_scrolloff_reveal(&self, travel_rows: isize, settle_row: usize) {
        // The gate is the tween engine, not a switch of its own: the carry is
        // made of tweens, so with the engine off the feature is inert and must
        // not touch any state.
        if !self.scroll_animation_enabled() || travel_rows == 0 {
            return;
        }

        let mut state = *self.key_inertia.borrow();
        if state.rows != 0 && (travel_rows > 0) != (state.rows > 0) {
            state.rows = 0;
        }
        state.rows += travel_rows;
        state.settle_row = settle_row;
        state.last_reveal = Some(Instant::now());
        state.phase = KeyInertiaPhase::Collecting;
        *self.key_inertia.borrow_mut() = state;
    }

    /// Forget every byte of cursor-follow inertia, carry included.
    fn reset_key_inertia(&self) {
        *self.key_inertia.borrow_mut() = KeyInertia::default();
    }

    /// Drop the cursor-follow inertia because something else has taken the view:
    /// a reveal that snapped, or a carry whose tween was killed underneath it.
    ///
    /// A no-op while the tween engine is off, which is also the state
    /// [`Self::set_scroll_animation_enabled`] leaves the feature in when it
    /// turns the engine off, so the disabled configuration never writes this
    /// cell at all.
    pub(crate) fn clear_scrolloff_inertia(&self) {
        if !self.scroll_animation_enabled() {
            return;
        }
        self.reset_key_inertia();
    }

    /// Set the scroll position from an external view sync while retaining a
    /// local sub-row pixel offset if both positions point at the same top row.
    ///
    /// # The same-row case must not touch an in-flight tween
    ///
    /// Helix owns cursor correctness, so a sync that *moves* the top row has to
    /// win over a stale animation target: it cancels the tween and lands on the
    /// incoming row, and the next crossing-gated sync re-reports that row to
    /// Helix.
    ///
    /// When `preserved_subrow` is true the resolved `y` **is** `current.y`, so
    /// the call provably changes nothing vertically. A sync with a nil vertical
    /// effect therefore has no justification for destroying a live tween, and
    /// must leave it running. The unconditional cancel that used to sit above the
    /// `preserved_subrow` test killed every in-flight scroll animation on any
    /// frame where Helix's stored row had not yet moved — the traced `page_down`
    /// flight lived 2.85ms, covered 2.5px of 728px, and died with
    /// `current_row = 0 incoming_row = 0`.
    ///
    /// A cancel against an absent tween is already a silent no-op, so the
    /// not-arming case is unchanged either way and needs no extra guard.
    pub(crate) fn set_scroll_position_from_view_sync_preserving_subrow_offset(
        &self,
        position: Point<Pixels>,
    ) {
        // Read the pre-cancel state so the trace can say whether a tween was
        // actually running, and the row comparison so the cancel below can be
        // gated on it. `cancel` does not touch `scroll_position`, and
        // `pixels_to_anchor` depends only on the extents, so hoisting these reads
        // above the cancel does not change what they compute.
        let tween_active = self.animation.is_active();
        let current = self.scroll_position.get();
        let current_line = self.pixels_to_anchor(current.y);
        let incoming_line = self.pixels_to_anchor(position.y);
        let preserved_subrow = current_line == incoming_line;

        // Helix owns cursor correctness. When it pushes a new view position
        // (typing, `page_down` inside the editor, a document edit) that has to
        // win over a stale animation target instead of being animated back.
        // That justification only exists once the top row actually changes;
        // see the doc comment for why the same-row case must be left alone.
        if !preserved_subrow {
            self.animation.cancel("helix_view_sync_row_change");
        }

        let y = if preserved_subrow {
            current.y
        } else {
            position.y
        };

        // The trace is split by outcome so the next bisect reads itself: a
        // `row_change` is a tween destroyed with a cause, a `same_row` is a
        // tween this sync deliberately spared. `tween_active` is carried on both
        // so the spared case is distinguishable from a genuinely idle sync.
        let reason = if preserved_subrow {
            "helix_view_sync_same_row"
        } else {
            "helix_view_sync_row_change"
        };
        trace!(
            scroll_manager_id = self.id,
            tween_active,
            current_row = current_line,
            incoming_row = incoming_line,
            preserved_subrow,
            incoming_y = ?position.y,
            resolved_y = ?y,
            reason,
            "ScrollManager view sync"
        );

        self.set_scroll_position_internal(point(position.x, y), false, "helix_view_sync");
    }

    /// Apply a horizontal-only update from a Helix view sync, leaving the vertical
    /// position and any in-flight vertical tween untouched.
    ///
    /// The horizontal axis cannot move the viewport vertically, so a horizontal
    /// sync has no business cancelling a vertical scroll tween. Routing it
    /// through [`Self::set_scroll_position_from_view_sync_preserving_subrow_offset`]
    /// did exactly that, on every painted frame, because that method cancelled
    /// unconditionally before the `preserved_subrow` test that now gates it.
    ///
    /// The sub-row preservation performed by that method is already a no-op for
    /// the horizontal caller: it passes the current y, so the incoming top line
    /// always equals the current top line and y resolves to itself. Dropping it
    /// therefore changes nothing except the cancel.
    ///
    /// With no tween in flight this is exactly what the old call did, because it
    /// is the same write:
    ///  - x is clamped into `0..=max_scroll_offset().width`,
    ///  - the position is stored only when it actually differs, and only then is
    ///    the "ScrollManager position changed" trace emitted,
    ///  - `pending_view_sync` is *not* armed, because this is a Helix -> native
    ///    sync (`from_native_view = false`), matching the old call.
    ///
    /// y is carried over verbatim rather than re-derived. Every position write
    /// goes through `clamp_position`, so the stored y is already inside
    /// `0..=max_scroll_offset().height` and re-clamping it is the identity; a
    /// dedicated axis-only path removes the question entirely.
    pub(crate) fn set_horizontal_scroll_offset_from_view_sync(&self, x: Pixels) {
        let current = self.scroll_position.get();
        self.set_scroll_position_internal(point(x, current.y), false, "helix_horizontal_offset");
    }

    /// Pixel distance scrolled within the current top line.
    pub(crate) fn vertical_offset_within_line(&self) -> Pixels {
        self.vertical_offset_within_line_at(self.scroll_position.get().y)
    }

    /// Pixel distance scrolled within the top line of an arbitrary position.
    pub(crate) fn vertical_offset_within_line_at(&self, position_y: Pixels) -> Pixels {
        let top_line = self.pixels_to_anchor(position_y);
        (position_y - self.anchor_to_pixels(top_line)).max(px(0.0))
    }

    /// Clamp a scroll position into the valid `0..=max` range on both axes.
    fn clamp_position(&self, position: Point<Pixels>) -> Point<Pixels> {
        let max_offset = self.max_scroll_offset();
        // Zed convention: positions are positive when scrolled, clamped between 0 and max
        point(
            position.x.max(px(0.0)).min(max_offset.width),
            position.y.max(px(0.0)).min(max_offset.height),
        )
    }

    /// Internal method to set scroll position with control over native sync tracking
    ///
    /// `reason` names the cause of the write. It is attached both to the tween
    /// cancel below and to the position-changed trace, so a position change can
    /// always be tied back to the interaction that caused it.
    fn set_scroll_position_internal(
        &self,
        position: Point<Pixels>,
        from_native_view: bool,
        reason: &'static str,
    ) {
        if from_native_view {
            // Direct manipulation (scrollbar drag, pointer placement) has to stay
            // 1:1, so any in-flight tween is dropped in favor of the pointer.
            self.animation.cancel(reason);
        }

        let clamped_position = self.clamp_position(position);
        let old_position = self.scroll_position.get();
        self.scroll_position.set(clamped_position);

        if old_position != clamped_position {
            trace!(
                scroll_manager_id = self.id,
                reason,
                old_position = ?old_position,
                new_position = ?clamped_position,
                from_native_view = from_native_view,
                "ScrollManager position changed"
            );
            if from_native_view {
                self.pending_view_sync.set(true);
            }
        }
    }

    fn clamp_scroll_position_after_extent_change(&self) {
        let old_position = self.scroll_position.get();
        let old_top_line = self.pixels_to_anchor(old_position.y);

        self.set_scroll_position_internal(old_position, false, "extent_change_reclamp");

        let new_position = self.scroll_position.get();
        if old_position != new_position {
            let new_top_line = self.pixels_to_anchor(new_position.y);
            if old_top_line != new_top_line || old_position.x != new_position.x {
                self.pending_view_sync.set(true);
            }
        }

        self.retarget_scroll_animation();
    }

    /// Re-anchor an in-flight tween after the position was re-clamped because
    /// the line height or the content extent changed. Without re-anchoring the
    /// tween would keep easing from a stale origin and visibly jitter. The
    /// extent setters bail out early when nothing changed, so this only runs for
    /// a real extent change.
    fn retarget_scroll_animation(&self) {
        self.animation.retarget_from(self.scroll_position.get().y);
        self.animation.clamp_target(self.max_scroll_offset().height);
    }

    /// Enable or disable smooth scrolling of discrete scroll requests.
    pub(crate) fn set_scroll_animation_enabled(&self, enabled: bool) {
        self.animation.set_enabled(enabled);
        if !enabled {
            // The cursor-follow inertia *is* a tween, so switching the engine off
            // strands it. Dropping it here is what makes the frame loop stop on
            // the next frame instead of waiting out a gesture window and then
            // trying to arm a tween it can no longer run.
            self.reset_key_inertia();
        }
    }

    pub(crate) fn scroll_animation_enabled(&self) -> bool {
        self.animation.enabled()
    }

    /// Whether the editor render pass has to keep requesting frames.
    ///
    /// This is deliberately a predicate of *desired* motion rather than of
    /// per-frame movement: a frame that produces no pixel change must still
    /// schedule the next one, otherwise a tween stalls and never completes.
    /// It covers three things, all of which terminate on their own:
    ///
    ///  - the scroll tween, which is the discrete jump, the in-gesture wheel
    ///    tween onto the accumulated target, the post-stop glide, or the
    ///    cursor-follow carry,
    ///  - a wheel gesture that has not yet gone idle, which needs the idle
    ///    window to elapse before it can arm (or decline to arm) its glide,
    ///  - a cursor-follow gesture that has not yet gone idle, or the carry of one
    ///    that has, which need the same window and the same engine deadline.
    ///
    /// The second and third terms are *deadlines*, not stalls: once
    /// `now - last_event` (or `now - last_reveal`) passes its idle window,
    /// [`Self::advance_wheel_glide`] and [`Self::advance_scrolloff_inertia`]
    /// clear the pending gesture unconditionally, so a gesture that ends below
    /// its arming thresholds leaves nothing pending here and the loop stops.
    ///
    /// The first term terminates on its own schedule and is not re-armed by the
    /// frame loop: only input re-arms it. Each arming hands the tween a fresh
    /// full-duration deadline, and `ScrollAnimation::advance` cancels it as
    /// soon as a sample reaches `t = 1`, so neither a gesture nor a carry can
    /// keep it alive indefinitely. Nothing in the frame path calls back into a
    /// reveal or any `animate_*`, so a tween is never extended by a frame.
    pub(crate) fn scroll_needs_frames(&self) -> bool {
        self.animation.is_active()
            || self.wheel_gesture_pending()
            || self.key_inertia_pending()
    }

    /// Whether a wheel gesture is still waiting for its idle window to elapse.
    fn wheel_gesture_pending(&self) -> bool {
        self.wheel_glide_enabled.get() && self.wheel_gesture.borrow().last_event.is_some()
    }

    /// Whether a cursor-follow gesture is still collecting, or its carry is in
    /// flight.
    ///
    /// Gated on the tween engine for the same reason
    /// [`Self::wheel_gesture_pending`] is gated on the wheel switch: with
    /// nothing to tween with, the whole feature is inert and must not hold the
    /// frame loop open.
    fn key_inertia_pending(&self) -> bool {
        self.scroll_animation_enabled()
            && self.key_inertia.borrow().phase != KeyInertiaPhase::Idle
    }

    pub(crate) fn scroll_animation_active(&self) -> bool {
        self.animation.is_active()
    }

    /// Drop any in-flight tween, used where correctness demands an instant jump.
    pub(crate) fn animation_cancel(&self, reason: &'static str) {
        self.animation.cancel(reason);
    }

    /// Tween the vertical scroll position toward `target` over `duration`.
    pub(crate) fn animate_scroll_to(&self, target: Point<Pixels>, duration: Duration) {
        let Some((from, to)) = self.resolve_scroll_tween(target) else {
            return;
        };
        self.animation.animate_to(from, to, duration);
    }

    /// The (from, to) legs of a vertical tween toward `target`, or `None` when
    /// there is no distance to travel.
    ///
    /// The no-distance case cancels rather than arms, which is the pre-existing
    /// behaviour of a scroll whose target is where it already is; keeping that
    /// in one place is what lets [`Self::animate_scroll_to`] and
    /// [`Self::animate_scroll_to_at`] differ only in the tween's origin. The
    /// legs are the *live* position and the *clamped* target: a tween eases
    /// away from the position that is actually on screen.
    fn resolve_scroll_tween(&self, target: Point<Pixels>) -> Option<(Pixels, Pixels)> {
        let clamped = self.clamp_position(target);
        let from = self.scroll_position.get().y;
        if clamped.y == from {
            self.animation.cancel("scroll_target_already_reached");
            return None;
        }
        Some((from, clamped.y))
    }

    /// Tween toward `target` over `duration`, starting at `start`.
    ///
    /// `animate_scroll_to` is this with the current clock. Taking the origin
    /// explicitly is what makes the whole glide sequence — idle window
    /// elapsing, glide arming, glide flight — reproducible from an injected
    /// `Instant` in a test, with no dependence on how long the arming call
    /// itself happened to take.
    pub(crate) fn animate_scroll_to_at(
        &self,
        target: Point<Pixels>,
        duration: Duration,
        start: Instant,
    ) {
        let Some((from, to)) = self.resolve_scroll_tween(target) else {
            return;
        };
        self.animation.animate_to_at(from, to, duration, start);
    }

    /// Enable or disable the eased glide that follows a wheel gesture.
    ///
    /// Disabling drops any pending gesture, which is what makes the change take
    /// effect at runtime with no config reload: an in-flight glide tween is left
    /// to finish on its own (it is bounded by `GLIDE_MS` and cannot outlive the
    /// frame loop), but a pending gesture is dropped immediately so the frame
    /// loop stops on the next frame.
    pub(crate) fn set_wheel_glide_enabled(&self, enabled: bool) {
        self.wheel_glide_enabled.set(enabled);
        if !enabled {
            self.clear_wheel_gesture();
        }
    }

    pub(crate) fn wheel_glide_enabled(&self) -> bool {
        self.wheel_glide_enabled.get()
    }

    /// Forget the pending gesture entirely. After this no gesture is pending,
    /// whatever the threshold arithmetic would have said.
    fn clear_wheel_gesture(&self) {
        *self.wheel_gesture.borrow_mut() = WheelGesture::default();
    }

    /// Advance the pending wheel gesture to `now`, arming its glide if the
    /// gesture is over and was significant. Returns whether a glide was armed.
    ///
    /// Call this once per rendered frame, for as long as
    /// [`Self::scroll_needs_frames`] is true. The three outcomes are:
    ///
    ///  - no gesture pending, or the wheel is still within the idle window:
    ///    return false and change nothing. The caller keeps requesting frames,
    ///    because the gesture is still going to need a decision.
    ///  - the gesture is over but below the arming thresholds: clear it and
    ///    return false. The clear is **unconditional** — a gesture that
    ///    survives this point would keep `scroll_needs_frames()` true forever
    ///    and the frame loop would never terminate. That is the single
    ///    highest-risk failure in this feature, so the clear does not depend on
    ///    the threshold outcome.
    ///  - the gesture is over and significant: clear it, then tween a glide of
    ///    `clamp(accumulated * RATIO, ±MAX_PX)` past the current position over
    ///    `GLIDE_MS`, starting at `now` so the flight is anchored to the
    ///    decision rather than to a clock read inside the tween.
    pub(crate) fn advance_wheel_glide(&self, now: Instant) -> bool {
        if !self.wheel_glide_enabled.get() {
            return false;
        }

        let (accumulated, events, last_event) = {
            let gesture = self.wheel_gesture.borrow();
            let Some(last_event) = gesture.last_event else {
                return false;
            };
            (gesture.accumulated, gesture.events, last_event)
        };

        if now.saturating_duration_since(last_event) < wheel_glide::GESTURE_IDLE {
            return false;
        }

        // The gesture is over. It is dropped before the thresholds are even
        // consulted, so neither an insignificant gesture nor one with the
        // feature switched off mid-gesture can leave a frame loop running.
        self.clear_wheel_gesture();

        let significant =
            events >= wheel_glide::ARM_MIN_EVENTS && accumulated.abs() >= wheel_glide::ARM_MIN_PX;
        if !significant {
            return false;
        }

        // The glide is an eased tween, so it is subject to the same engine gate
        // as every other tween: with the engine off there is nothing to tween
        // with. Bailing out here rather than letting the arming call refuse is
        // deliberate — refusing would also *cancel* whatever tween happened to
        // be in flight (a discrete page jump armed during the idle window),
        // turning a glide that cannot run into a jump that cannot finish.
        if !self.scroll_animation_enabled() {
            return false;
        }

        // Sign spaces. `accumulated` is the sum of raw GPUI `delta.y` values, where
        // NEGATIVE means scrolling down. `scroll_position().y` is the opposite: a
        // larger position means further down the document. The two must not be
        // added together directly — doing so sends a downward glide upward. The
        // negation converts the wheel's convention into the position's, so the
        // glide always continues the direction the wheel was already travelling.
        let glide = px(-accumulated * wheel_glide::RATIO)
            .max(-wheel_glide::MAX_PX)
            .min(wheel_glide::MAX_PX);
        let current = self.scroll_position.get();
        self.animate_scroll_to_at(
            point(current.x, current.y + glide),
            wheel_glide::GLIDE_DURATION,
            now,
        );
        // Read liveness back rather than assuming the arming took: a glide at
        // the very top or bottom of the document clamps onto the position the
        // gesture already reached, which cancels instead of arming. The frame
        // loop still ends either way — the gesture is already cleared and there
        // is no tween to keep alive.
        self.animation.is_active()
    }

    /// Advance the cursor-follow inertia to `now`, arming the carry if one is
    /// due. Returns whether a carry was armed.
    ///
    /// Call this once per rendered frame, for as long as
    /// [`Self::scroll_needs_frames`] is true. The phases and their outcomes:
    ///
    ///  - `Idle`: nothing to do.
    ///  - `Collecting`, still inside the idle window: return false and change
    ///    nothing. The caller keeps requesting frames, because the gesture is
    ///    still going to need a decision.
    ///  - `Collecting`, the window elapsed: **clear the gesture unconditionally**,
    ///    above every threshold check, then arm the carry if the gesture travelled
    ///    far enough. That clear is the termination guarantee — a gesture that
    ///    survived this point would keep `scroll_needs_frames()` true forever and
    ///    the frame loop would never terminate. It is the single highest-risk
    ///    failure in the feature, so it does not depend on the threshold outcome
    ///    or on whether the arming below ends up arming anything.
    ///  - `Outward`, carry still flying: do nothing. The carry is a bounded tween
    ///    and the engine ends it.
    ///  - `Outward`, carry gone: the feature is over, whether it landed on its
    ///    recorded target or was interrupted by something with a stronger claim
    ///    on the view — a native scrollbar drag, a Helix view sync, a wheel
    ///    notch, the engine being switched off. Both go to `Idle`, because with a
    ///    single leg there is nothing left to arm; the landing check only decides
    ///    which of the two the trace reports. An interrupted carry is not a stall
    ///    either: the tween that was killed is gone, so the *next* frame's
    ///    `key_inertia_pending()` is already false and the loop stops instead of
    ///    spinning on a tween that no longer exists.
    ///
    /// The carry is an ordinary tween, so the frame driver samples it through
    /// [`Self::advance_scroll_animation_at`] exactly like any other tween — this
    /// method only decides what comes next, and only when nothing is flying.
    pub(crate) fn advance_scrolloff_inertia(&self, now: Instant) -> bool {
        if !self.scroll_animation_enabled() {
            return false;
        }

        // Worked on as a copy and written back once, so the `RefCell` is never
        // held across an arming call. `advance_wheel_glide` reads its state out
        // for the same reason.
        let mut state = *self.key_inertia.borrow();
        let armed = self.step_key_inertia(&mut state, now);
        *self.key_inertia.borrow_mut() = state;
        armed
    }

    /// One step of [`Self::advance_scrolloff_inertia`], on a caller-owned copy
    /// of the state.
    fn step_key_inertia(&self, state: &mut KeyInertia, now: Instant) -> bool {
        match state.phase {
            KeyInertiaPhase::Idle => false,

            KeyInertiaPhase::Collecting => {
                let Some(last_reveal) = state.last_reveal else {
                    // Unreachable by construction: `Collecting` is only ever
                    // entered together with a timestamp. Treated as a clear
                    // rather than an `unreachable!()` so a future edit that gets
                    // it wrong ends the frame loop instead of panicking inside a
                    // frame.
                    *state = KeyInertia::default();
                    return false;
                };
                if now.saturating_duration_since(last_reveal) < key_inertia::GESTURE_IDLE {
                    return false;
                }

                // The gesture is over. It is dropped before the threshold is
                // even consulted, so neither an insignificant gesture nor a
                // feature switched off mid-gesture can leave a frame loop
                // running. The clear covers the whole struct, carry included.
                let travelled = state.rows;
                let settle_row = state.settle_row;
                *state = KeyInertia::default();

                if (travelled.abs() as f32) < key_inertia::MIN_GESTURE_ROWS {
                    return false;
                }
                // A carry *is* a tween, so it needs the engine. That is already
                // guaranteed by the gate at the top of
                // `advance_scrolloff_inertia`; it is not re-tested here because
                // this function has no other caller, and duplicating the check
                // would invite the two to drift.
                self.arm_scrolloff_overshoot(state, travelled, settle_row, now)
            }

            KeyInertiaPhase::Outward => {
                if self.animation.is_active() {
                    return false;
                }
                // The tween is gone, so there is nothing left to fly: with one
                // leg this is the end of the feature either way. It is worth
                // knowing *which* way, because the two mean different things —
                // a landing is the carry finishing as designed, while a position
                // away from the recorded target means something with a stronger
                // claim on the view took it (a native scrollbar drag, a Helix
                // view sync, a wheel notch) and the feature must not fight that.
                // The check therefore decides what the trace says; it no longer
                // decides what the state says, because there is no second leg
                // that only a landing is allowed to arm.
                let landed = (self.scroll_position.get().y - state.outward_target).abs()
                    <= key_inertia::LANDED_EPSILON;
                trace!(
                    landed,
                    target_y = ?state.outward_target,
                    "cursor-follow carry ended"
                );
                *state = KeyInertia::default();
                false
            }
        }
    }

    /// Arm the carry for a gesture that has ended and travelled far enough.
    /// `travelled` is its signed row travel and `settle_row` the exact
    /// `scrolloff`-margin row its last reveal eased onto, which is the row the
    /// carry is measured from.
    fn arm_scrolloff_overshoot(
        &self,
        state: &mut KeyInertia,
        travelled: isize,
        settle_row: usize,
        now: Instant,
    ) -> bool {
        // ### Why carrying is safe
        //
        // The carry always moves the view *further along the direction the
        // gesture was already travelling*, and that is precisely the one
        // direction in which the cursor retreats into the viewport rather than
        // being pushed toward an edge:
        //
        //  - scrolling DOWN, the position grows, so the cursor's offset within
        //    the viewport *decreases* — away from the bottom edge it had come to
        //    rest against,
        //  - scrolling UP, the position shrinks, so the offset *increases* —
        //    away from the top edge.
        //
        // Inverting this sign would turn the whole feature into a way to push
        // the cursor off the bottom of the screen, which is the one outcome
        // that must never ship. That is why the direction is asserted in the
        // tests against the cursor's offset within the viewport, and not only
        // against the sign of the position.
        //
        // ### Why the margin cannot ratchet
        //
        // There is no return leg any more, so the obvious worry is that the
        // margin creeps outward gesture after gesture until the configured
        // `scrolloff` is fiction. It is bounded, and the reason is that the
        // carry's base is the *fresh* margin row of the gesture's last reveal,
        // not a running total. Worked through with `visible_rows = 40` and
        // `margin = 5`, so the band fires forward at
        // `cursor >= top + visible_rows - margin = top + 35` and the reveal's
        // target row is `cursor + margin + 1 - visible_rows = cursor - 34`:
        //
        // ```
        // reveal fires   cursor >= top + 35, target row `cursor - 34`, so the
        //                cursor ends up 34 rows below the top — `margin = 5`
        //                rows above the last visible row
        // carry 2 rows   top += 2, so the cursor is 32 rows below the top, i.e.
        //                7 rows from the bottom edge
        // next fire      the band only fires again at `cursor >= top + 35`, so
        //                the cursor has to walk back in by 3 rows first. Its
        //                target is then `cursor - 34` *again*, and the carry is
        //                measured from that row again — never from where the
        //                previous carry left the pixels
        // carry 2 rows   7 rows from the bottom edge again, and again after that
        // ```
        //
        // In general, after a carry the cursor sits
        // `(target_row + carry_rows + visible_rows - 1) - cursor`, and
        // `target_row = cursor - visible_rows + margin + 1`, so that distance is
        // exactly `margin + carry_rows`. It depends on *this* round's carry and
        // nothing that came before: each reveal re-anchors the band to the
        // current `dest_row`, so the previous carry is already inside the base
        // rather than added to it. And `carry_rows = min(rows * RATIO, MAX)` is
        // capped at [`key_inertia::OVERSHOOT_MAX`], so the distance can never
        // exceed `margin + OVERSHOOT_MAX` — 7 rows here, for a capped gesture,
        // every single time. A test pins exactly that across three rounds
        // (`repeated_carries_hold_the_margin_instead_of_ratcheting`).
        //
        // The same arithmetic is the reason the carry is measured from
        // `settle_row` rather than from the live position: an eased reveal is
        // still short of its target when the idle window elapses, and measuring
        // from where the pixels happen to be would make the carry's length depend
        // on how long the reveal was.
        let carry_rows = (travelled.abs() as f32 * key_inertia::OVERSHOOT_RATIO)
            .min(key_inertia::OVERSHOOT_MAX);
        let carry = carry_rows * self.line_height();
        let direction = if travelled > 0 { 1.0 } else { -1.0 };
        let settle_px = self.anchor_to_pixels(settle_row);
        let current = self.scroll_position.get();

        self.animate_scroll_to_at(
            point(current.x, settle_px + direction * carry),
            key_inertia::OVERSHOOT_DURATION,
            now,
        );

        // Liveness is read back rather than assumed, and the landing target with
        // it: a carry at the very top or bottom of the document clamps onto the
        // margin the gesture already reached, which cancels instead of arming.
        // Entering `Outward` with no tween would be a phase the frame loop could
        // never advance, so the whole feature is dropped instead — and the
        // recorded target is the *clamped* one, so "did it land" is asked about
        // where the carry will really stop.
        match self.animation.destination() {
            Some(landed) => {
                state.phase = KeyInertiaPhase::Outward;
                state.outward_target = landed;
                true
            }
            None => {
                *state = KeyInertia::default();
                false
            }
        }
    }

    /// The animation destination while a tween is in flight, otherwise the live
    /// scroll position. Callers that report scroll updates to Helix must use
    /// this so a tween reports where it is going, not where it currently is.
    pub(crate) fn destination_position(&self) -> Point<Pixels> {
        let current = self.scroll_position.get();
        match self.animation.destination() {
            Some(destination) => point(current.x, destination),
            None => current,
        }
    }

    /// Advance the in-flight tween using the current clock.
    pub(crate) fn advance_scroll_animation(&self) -> bool {
        self.advance_scroll_animation_at(Instant::now())
    }

    /// Advance the in-flight tween to `now` and return whether the scroll
    /// position changed.
    ///
    /// This reuses the wheel sync contract verbatim: the paint-time
    /// `pending_view_sync` channel is the only path back to Helix, and it is
    /// armed once a whole visual row has been crossed (or the horizontal offset
    /// moved). Sub-row motion stays local to the GUI viewport.
    pub(crate) fn advance_scroll_animation_at(&self, now: Instant) -> bool {
        let Some(destination) = self.animation.advance(now) else {
            return false;
        };

        let old_position = self.scroll_position.get();
        let old_line = self.pixels_to_anchor(old_position.y);

        self.set_scroll_position_internal(
            point(old_position.x, destination),
            false,
            "scroll_animation_advance",
        );

        let new_position = self.scroll_position.get();
        let new_line = self.pixels_to_anchor(new_position.y);
        let crossed_lines = new_line as isize - old_line as isize;
        if crossed_lines != 0 || old_position.x != new_position.x {
            self.pending_view_sync.set(true);
        }

        old_position != new_position
    }

    /// Convert a pixel scroll offset to a Helix viewport anchor (line number)
    pub(crate) fn pixels_to_anchor(&self, y: Pixels) -> usize {
        let line_height = self.line_height.get();
        let total_lines = self.total_lines.get();
        let line = (y / line_height).floor() as usize;
        line.min(total_lines.saturating_sub(1))
    }

    /// Convert a Helix viewport anchor (line number) to pixel scroll offset
    pub(crate) fn anchor_to_pixels(&self, anchor: usize) -> Pixels {
        let line_height = self.line_height.get();
        line_height * (anchor as f32)
    }
}

#[cfg(test)]
mod scroll_manager_tests {
    use super::*;

    #[test]
    fn test_scroll_manager_creation() {
        let line_height = px(20.0);
        let manager = ScrollManager::new(line_height);

        assert_eq!(manager.line_height(), line_height);
        assert_eq!(manager.total_lines(), 1);
        assert_eq!(manager.scroll_position(), point(px(0.0), px(0.0)));
        assert!(!manager.has_pending_view_sync());
    }

    #[test]
    fn test_scroll_position_and_offset_conversion() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100); // Allow scrolling up to 100 lines
        manager.set_viewport_size(size(px(800.0), px(400.0))); // 400px viewport

        // Test positive position (Zed convention) - only vertical since horizontal is clamped to 0
        let position = point(px(0.0), px(100.0)); // X clamped to 0, Y should work
        manager.set_scroll_position(position, "test");
        assert_eq!(manager.scroll_position(), position);

        // Test offset conversion (negative for GPUI)
        let expected_offset = point(px(0.0), px(-100.0)); // X clamped to 0
        assert_eq!(manager.scroll_offset(), expected_offset);

        // Test setting offset (should convert to positive position)
        let offset = point(px(0.0), px(-150.0)); // Only vertical scrolling
        manager.set_scroll_offset(offset, "test");
        let expected_position = point(px(0.0), px(150.0));
        assert_eq!(manager.scroll_position(), expected_position);
    }

    #[test]
    fn test_scroll_by_delta_accumulates_subline_wheel_motion() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100);
        manager.set_viewport_size(size(px(800.0), px(400.0)));

        let (changed, crossed_lines) = manager.scroll_by_delta(point(px(0.0), px(-5.0)));
        assert!(changed);
        assert_eq!(crossed_lines, 0);
        assert_eq!(manager.scroll_position(), point(px(0.0), px(5.0)));
        assert_eq!(manager.vertical_offset_within_line(), px(5.0));
        assert!(!manager.has_pending_view_sync());

        let (_, crossed_lines) = manager.scroll_by_delta(point(px(0.0), px(-15.0)));
        assert_eq!(crossed_lines, 1);
        assert_eq!(manager.scroll_position(), point(px(0.0), px(20.0)));
        assert_eq!(manager.vertical_offset_within_line(), px(0.0));
        assert!(manager.has_pending_view_sync());
    }

    #[test]
    fn test_view_sync_preserves_subrow_offset_for_same_top_row() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100);
        manager.set_viewport_size(size(px(800.0), px(400.0)));

        manager.scroll_by_delta(point(px(0.0), px(-25.0)));
        assert_eq!(manager.scroll_position(), point(px(0.0), px(25.0)));
        assert_eq!(manager.vertical_offset_within_line(), px(5.0));

        manager
            .set_scroll_position_from_view_sync_preserving_subrow_offset(point(px(0.0), px(20.0)));
        assert_eq!(manager.scroll_position(), point(px(0.0), px(25.0)));
        assert_eq!(manager.vertical_offset_within_line(), px(5.0));

        manager
            .set_scroll_position_from_view_sync_preserving_subrow_offset(point(px(0.0), px(40.0)));
        assert_eq!(manager.scroll_position(), point(px(0.0), px(40.0)));
        assert_eq!(manager.vertical_offset_within_line(), px(0.0));
    }

    #[test]
    fn test_viewport_resize_clamps_bottom_scroll_position() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100);
        manager.set_viewport_size(size(px(800.0), px(100.0)));
        manager.set_scroll_position(point(px(0.0), px(1900.0)), "test");
        manager.clear_pending_view_sync();

        manager.set_viewport_size(size(px(800.0), px(200.0)));

        assert_eq!(manager.scroll_position(), point(px(0.0), px(1800.0)));
        assert_eq!(manager.pixels_to_anchor(manager.scroll_position().y), 90);
        assert_eq!(manager.vertical_offset_within_line(), px(0.0));
        assert!(manager.has_pending_view_sync());
    }

    #[test]
    fn test_content_resize_clamps_bottom_scroll_position() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100);
        manager.set_viewport_size(size(px(800.0), px(100.0)));
        manager.set_scroll_position(point(px(0.0), px(1900.0)), "test");
        manager.clear_pending_view_sync();

        manager.set_total_lines(20);

        assert_eq!(manager.scroll_position(), point(px(0.0), px(300.0)));
        assert_eq!(manager.pixels_to_anchor(manager.scroll_position().y), 15);
        assert_eq!(manager.vertical_offset_within_line(), px(0.0));
        assert!(manager.has_pending_view_sync());
    }

    #[test]
    fn test_line_height_change_preserves_visual_row_and_subrow_offset() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100);
        manager.set_viewport_size(size(px(800.0), px(100.0)));
        manager.set_scroll_position(point(px(0.0), px(105.0)), "test");
        manager.clear_pending_view_sync();

        manager.set_line_height(px(30.0));

        assert_eq!(manager.scroll_position(), point(px(0.0), px(157.5)));
        assert_eq!(manager.pixels_to_anchor(manager.scroll_position().y), 5);
        assert_eq!(manager.vertical_offset_within_line(), px(7.5));
        assert!(!manager.has_pending_view_sync());
    }

    #[test]
    fn test_pixels_to_anchor_conversion() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100);

        // Test basic conversions
        assert_eq!(manager.pixels_to_anchor(px(0.0)), 0);
        assert_eq!(manager.pixels_to_anchor(px(20.0)), 1);
        assert_eq!(manager.pixels_to_anchor(px(40.0)), 2);
        assert_eq!(manager.pixels_to_anchor(px(100.0)), 5);

        // Test fractional pixels (should floor)
        assert_eq!(manager.pixels_to_anchor(px(19.9)), 0);
        assert_eq!(manager.pixels_to_anchor(px(20.1)), 1);
        assert_eq!(manager.pixels_to_anchor(px(39.9)), 1);

        // Test clamping to total lines
        assert_eq!(manager.pixels_to_anchor(px(2000.0)), 99); // Should clamp to last line (99)
    }

    #[test]
    fn test_anchor_to_pixels_conversion() {
        let manager = ScrollManager::new(px(20.0));

        // Test basic conversions
        assert_eq!(manager.anchor_to_pixels(0), px(0.0));
        assert_eq!(manager.anchor_to_pixels(1), px(20.0));
        assert_eq!(manager.anchor_to_pixels(5), px(100.0));
        assert_eq!(manager.anchor_to_pixels(10), px(200.0));
    }

    #[test]
    fn test_round_trip_anchor_pixel_conversion() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(50);

        // Test round-trip conversions
        for line in [0, 1, 5, 10, 25, 49] {
            let pixels = manager.anchor_to_pixels(line);
            let recovered_line = manager.pixels_to_anchor(pixels);
            assert_eq!(recovered_line, line);
        }

        // Test pixel round-trips (may differ due to flooring)
        for pixels in [px(0.0), px(20.0), px(40.0), px(100.0), px(200.0)] {
            let line = manager.pixels_to_anchor(pixels);
            let recovered_pixels = manager.anchor_to_pixels(line);
            assert_eq!(recovered_pixels, pixels);
        }
    }

    #[test]
    fn test_max_scroll_offset_calculation() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100); // 100 lines * 20px = 2000px content height
        manager.set_viewport_size(size(px(800.0), px(400.0))); // 400px viewport height

        let max_offset = manager.max_scroll_offset();

        // Max scroll should be content_height - viewport_height = 2000 - 400 = 1600px
        assert_eq!(max_offset.height, px(1600.0));
        assert_eq!(max_offset.width, px(0.0)); // No horizontal scrolling in this test
    }

    #[test]
    fn test_horizontal_scroll_offset_calculation() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_viewport_size(size(px(800.0), px(400.0)));
        manager.set_content_width(px(1200.0));

        let max_offset = manager.max_scroll_offset();

        assert_eq!(max_offset.width, px(400.0));
    }

    #[test]
    fn test_max_scroll_offset_clamping_to_zero() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(10); // 10 lines * 20px = 200px content height
        manager.set_viewport_size(size(px(800.0), px(400.0))); // 400px viewport height (larger than content)

        let max_offset = manager.max_scroll_offset();

        // Max scroll should be clamped to 0 when content is smaller than viewport
        assert_eq!(max_offset.height, px(0.0));
        assert_eq!(max_offset.width, px(0.0));
    }

    #[test]
    fn test_scroll_position_clamping() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(50); // 50 lines * 20px = 1000px
        manager.set_viewport_size(size(px(800.0), px(400.0))); // Max scroll = 1000 - 400 = 600px

        // Test normal position (should not clamp)
        manager.set_scroll_position(point(px(0.0), px(300.0)), "test");
        assert_eq!(manager.scroll_position(), point(px(0.0), px(300.0)));

        // Test negative position (should clamp to 0)
        manager.set_scroll_position(point(px(-50.0), px(-100.0)), "test");
        assert_eq!(manager.scroll_position(), point(px(0.0), px(0.0)));

        // Test position beyond max (should clamp to max)
        manager.set_scroll_position(point(px(0.0), px(800.0)), "test");
        assert_eq!(manager.scroll_position(), point(px(0.0), px(600.0))); // Clamped to max
    }

    #[test]
    fn test_horizontal_scroll_position_clamping() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_viewport_size(size(px(800.0), px(400.0)));
        manager.set_content_width(px(1200.0));

        manager.set_scroll_position(point(px(500.0), px(0.0)), "test");

        assert_eq!(manager.scroll_position(), point(px(400.0), px(0.0)));
    }

    #[test]
    fn test_horizontal_extent_clamp_marks_pending_sync() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_viewport_size(size(px(800.0), px(400.0)));
        manager.set_content_width(px(1200.0));
        manager.set_scroll_position(point(px(400.0), px(0.0)), "test");
        manager.clear_pending_view_sync();

        manager.set_content_width(px(900.0));

        assert_eq!(manager.scroll_position(), point(px(100.0), px(0.0)));
        assert!(manager.has_pending_view_sync());
    }

    #[test]
    fn test_horizontal_scroll_delta_marks_pending_sync() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_viewport_size(size(px(800.0), px(400.0)));
        manager.set_content_width(px(1200.0));

        let (changed, crossed_lines) = manager.scroll_by_delta(point(px(-40.0), px(0.0)));

        assert!(changed);
        assert_eq!(crossed_lines, 0);
        assert_eq!(manager.scroll_position(), point(px(40.0), px(0.0)));
        assert!(manager.has_pending_view_sync());
    }

    #[test]
    fn test_pending_view_sync_flag() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100); // Allow scrolling
        manager.set_viewport_size(size(px(800.0), px(400.0))); // Set viewport size

        // Initial state
        assert!(!manager.has_pending_view_sync());

        // Setting position from a native interaction should set the flag
        manager.set_scroll_position(point(px(0.0), px(50.0)), "test");
        assert!(manager.has_pending_view_sync());

        // Reset flag manually
        manager.clear_pending_view_sync();
        assert!(!manager.has_pending_view_sync());

        // Setting position from view sync should NOT set the flag
        manager
            .set_scroll_position_from_view_sync_preserving_subrow_offset(point(px(0.0), px(100.0)));
        assert!(!manager.has_pending_view_sync());

        // Setting offset from a native interaction should set the flag
        manager.set_scroll_offset(point(px(0.0), px(-150.0)), "test");
        assert!(manager.has_pending_view_sync());
    }

    #[test]
    fn test_basic_visual_row_conversion() {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(50);

        let scroll_position_y = px(200.0); // 10 lines down
        let anchor_line = manager.pixels_to_anchor(scroll_position_y);
        assert_eq!(anchor_line, 10);

        let pixels = manager.anchor_to_pixels(15);
        assert_eq!(pixels, px(300.0)); // 15 * 20px = 300px
    }

    fn scrollable_manager() -> ScrollManager {
        let mut manager = ScrollManager::new(px(20.0));
        manager.set_total_lines(100);
        manager.set_viewport_size(size(px(800.0), px(400.0)));
        manager
    }

    #[test]
    fn test_scroll_animation_is_disabled_by_default() {
        let manager = scrollable_manager();

        assert!(!manager.scroll_animation_enabled());
        assert!(!manager.scroll_animation_active());
        assert_eq!(manager.destination_position(), manager.scroll_position());
        assert!(!manager.advance_scroll_animation());
    }

    #[test]
    fn test_advance_scroll_animation_arms_view_sync_only_on_row_crossing() {
        let manager = scrollable_manager();
        manager.set_scroll_animation_enabled(true);
        let start = Instant::now();
        manager.animate_scroll_to(point(px(0.0), px(24.0)), Duration::from_millis(200));
        manager.clear_pending_view_sync();

        // Sub-row motion stays local, exactly like the wheel.
        assert!(manager.advance_scroll_animation_at(start + Duration::from_millis(20)));
        assert!(manager.scroll_position().y > px(0.0));
        assert!(manager.scroll_position().y < px(20.0));
        assert_eq!(
            manager.vertical_offset_within_line(),
            manager.scroll_position().y
        );
        assert!(!manager.has_pending_view_sync());
        assert!(manager.scroll_animation_active());

        // Crossing a whole visual row arms the paint-time Helix sync channel.
        // `start` is taken just before the tween is armed, so the sample has to
        // land past the tween's own 200ms deadline rather than exactly on it.
        assert!(manager.advance_scroll_animation_at(start + Duration::from_millis(240)));
        assert_eq!(manager.scroll_position(), point(px(0.0), px(24.0)));
        assert!(manager.has_pending_view_sync());
        assert!(!manager.scroll_animation_active());
    }

    #[test]
    fn test_advance_scroll_animation_reports_the_destination() {
        let manager = scrollable_manager();
        manager.set_scroll_animation_enabled(true);
        manager.animate_scroll_to(point(px(0.0), px(400.0)), Duration::from_millis(200));

        assert_eq!(manager.destination_position().y, px(400.0));

        manager.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500));

        assert_eq!(manager.scroll_position().y, px(400.0));
        assert_eq!(manager.destination_position().y, px(400.0));
    }

    #[test]
    fn test_set_scroll_position_cancels_in_flight_scroll_animation() {
        let manager = scrollable_manager();
        manager.set_scroll_animation_enabled(true);
        manager.animate_scroll_to(point(px(0.0), px(400.0)), Duration::from_millis(200));
        assert!(manager.scroll_animation_active());

        manager.set_scroll_position(point(px(0.0), px(20.0)), "test");

        assert!(!manager.scroll_animation_active());
        assert_eq!(manager.scroll_position(), point(px(0.0), px(20.0)));
        assert_eq!(manager.destination_position(), point(px(0.0), px(20.0)));
        assert!(!manager.advance_scroll_animation());
    }

    #[test]
    fn test_view_sync_cancels_in_flight_scroll_animation() {
        let manager = scrollable_manager();
        manager.set_scroll_animation_enabled(true);
        manager.animate_scroll_to(point(px(0.0), px(400.0)), Duration::from_millis(200));
        assert!(manager.scroll_animation_active());

        manager
            .set_scroll_position_from_view_sync_preserving_subrow_offset(point(px(0.0), px(200.0)));

        assert!(!manager.scroll_animation_active());
        assert_eq!(manager.scroll_position(), point(px(0.0), px(200.0)));
    }

    /// A wheel event must take priority over an in-flight discrete jump, and the
    /// proof is what survives the sampling rather than whether a tween exists.
    ///
    /// The wheel now replaces the dead tween with its *own* tween, so
    /// `scroll_animation_active()` is true again immediately after the event.
    /// What must not survive is the 200ms `0 -> 400px` jump, and the two tween
    /// shapes are told apart by the sample they produce:
    ///
    ///  - the wheel's tween is `0 -> 5px` over `TWEEN_MS = 80ms`, so a sample at
    ///    `+40ms` is `t = 0.5`, `ease_out_quad(0.5) = 0.75`, and reads
    ///    `0 + 5 * 0.75 = 3.75px`;
    ///  - the page jump, had it survived, would be `0 -> 400px` over 200ms, and
    ///    the same `+40ms` sample would read `t = 0.2`, eased `0.36`, i.e. `144px`
    ///    — and it would still be in flight at `+80ms`, where the wheel's tween
    ///    has already landed.
    ///
    /// The landing on exactly `5px` at `+80ms` is the sharper of the two
    /// assertions: `0.2`-of-200ms is not a rounding artefact of a live tween, it
    /// is a completely different destination.
    #[test]
    fn test_scroll_by_delta_cancels_in_flight_scroll_animation() {
        let manager = scrollable_manager();
        manager.set_scroll_animation_enabled(true);
        manager.animate_scroll_to(point(px(0.0), px(400.0)), Duration::from_millis(200));
        assert!(manager.scroll_animation_active());

        let start = Instant::now();
        let (changed, crossed_lines) = manager.scroll_by_delta_at(point(px(0.0), px(-5.0)), start);

        assert!(changed);
        // 5px is inside row 0 on a 20px row, so the request crosses no row.
        assert_eq!(crossed_lines, 0);
        assert_eq!(
            manager.scroll_position().y,
            px(0.0),
            "the wheel teleported instead of easing onto the target"
        );

        assert!(manager.advance_scroll_animation_at(start + Duration::from_millis(40)));
        assert!(
            (manager.scroll_position().y - px(3.75)).abs() < px(0.01),
            "half-flight sample was {:?}, expected the wheel tween's 3.75px",
            manager.scroll_position().y
        );

        assert!(manager.advance_scroll_animation_at(start + Duration::from_millis(80)));
        assert_eq!(
            manager.scroll_position().y,
            px(5.0),
            "the surviving tween was the 200ms page jump, not the wheel's"
        );
        assert!(!manager.scroll_animation_active());
    }

    #[test]
    fn test_extent_change_keeps_scroll_animation_target_within_bounds() {
        let mut manager = scrollable_manager();
        manager.set_viewport_size(size(px(800.0), px(100.0)));
        manager.set_scroll_animation_enabled(true);
        manager.animate_scroll_to(point(px(0.0), px(1900.0)), Duration::from_millis(200));
        assert!(manager.scroll_animation_active());

        manager.set_total_lines(10);

        let max_y = manager.max_scroll_offset().height;
        assert_eq!(max_y, px(100.0));
        assert_eq!(manager.destination_position().y, max_y);
        assert!(manager.scroll_animation_active());
    }

    #[test]
    fn test_extent_change_drops_scroll_animation_without_room_to_move() {
        let mut manager = scrollable_manager();
        manager.set_viewport_size(size(px(800.0), px(100.0)));
        manager.set_scroll_animation_enabled(true);
        manager.animate_scroll_to(point(px(0.0), px(500.0)), Duration::from_millis(200));
        assert!(manager.scroll_animation_active());

        manager.set_viewport_size(size(px(800.0), px(4000.0)));

        assert_eq!(manager.max_scroll_offset().height, px(0.0));
        assert!(!manager.scroll_animation_active());
        assert_eq!(manager.destination_position(), point(px(0.0), px(0.0)));
    }

    #[test]
    fn test_line_height_change_reanchors_in_flight_scroll_animation() {
        let mut manager = scrollable_manager();
        manager.set_viewport_size(size(px(800.0), px(100.0)));
        manager.set_scroll_animation_enabled(true);
        manager.set_scroll_position(point(px(0.0), px(100.0)), "test");
        manager.clear_pending_view_sync();
        manager.animate_scroll_to(point(px(0.0), px(1900.0)), Duration::from_millis(200));

        manager.set_line_height(px(30.0));

        // The tween re-anchors onto the rescaled live position instead of easing
        // from the stale 20px-row origin, so the next sample cannot be below it.
        assert_eq!(manager.scroll_position().y, px(150.0));
        assert_eq!(manager.destination_position().y, px(1900.0));
        assert!(manager.advance_scroll_animation_at(Instant::now()));
        assert!(manager.scroll_position().y >= px(150.0));
        assert!(manager.scroll_animation_active());
        assert!(!manager.has_pending_view_sync());

        // Re-anchoring carries the remaining budget over instead of restarting
        // the full 200ms, so the tween still completes.
        assert!(manager.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500)));
        assert_eq!(manager.scroll_position().y, px(1900.0));
        assert!(!manager.scroll_animation_active());
    }

    #[test]
    fn test_animate_scroll_to_is_a_noop_without_distance_to_travel() {
        let manager = scrollable_manager();
        manager.set_scroll_animation_enabled(true);
        manager.set_scroll_position(point(px(0.0), px(20.0)), "test");

        manager.animate_scroll_to(point(px(0.0), px(20.0)), Duration::from_millis(200));

        assert!(!manager.scroll_animation_active());
        assert_eq!(manager.scroll_position(), point(px(0.0), px(20.0)));
    }

    #[test]
    fn test_unchanged_extent_setters_do_not_disturb_an_in_flight_scroll_animation() {
        let mut manager = scrollable_manager();
        manager.set_scroll_animation_enabled(true);
        manager.animate_scroll_to(point(px(0.0), px(400.0)), Duration::from_millis(200));
        // Taken just after the tween is armed, so `base + 240ms` is safely past
        // the tween's own 200ms deadline.
        let base = Instant::now();

        // A stable layout re-sends all four extents on every painted frame. If any
        // of them re-anchored unconditionally it would reset the tween clock, and
        // the tween could never advance past `t = 0`.
        manager.set_line_height(px(20.0));
        manager.set_total_lines(100);
        // This fixture never sets a content width, so 0 is the current value.
        manager.set_content_width(px(0.0));
        manager.set_viewport_size(size(px(800.0), px(400.0)));
        assert!(manager.scroll_animation_active());

        // Let the tween travel, then re-send the extents and sample the *same*
        // instant again. The position has to be untouched: a reset clock would
        // re-anchor onto the travelled position and move it.
        let midway = base + Duration::from_millis(100);
        assert!(manager.advance_scroll_animation_at(midway));
        let travelled = manager.scroll_position().y;
        assert!(travelled > px(0.0));

        manager.set_line_height(px(20.0));
        manager.set_total_lines(100);
        manager.set_content_width(px(0.0));
        manager.set_viewport_size(size(px(800.0), px(400.0)));

        assert!(!manager.advance_scroll_animation_at(midway));
        assert_eq!(manager.scroll_position().y, travelled);

        // And the original 200ms deadline still completes it exactly.
        assert!(manager.advance_scroll_animation_at(base + Duration::from_millis(240)));
        assert_eq!(manager.scroll_position().y, px(400.0));
        assert!(!manager.scroll_animation_active());
    }

    #[test]
    fn test_scroll_needs_frames_tracks_tween_liveness() {
        let manager = scrollable_manager();
        assert!(!manager.scroll_needs_frames());

        let mut manager = scrollable_manager();
        manager.set_scroll_animation_enabled(true);
        assert!(!manager.scroll_needs_frames());

        manager.animate_scroll_to(point(px(0.0), px(400.0)), Duration::from_millis(200));

        // Frames are needed from the moment the tween is armed, not only when a
        // frame happens to move pixels.
        assert!(manager.scroll_needs_frames());
        assert!(manager.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500)));
        assert!(!manager.scroll_needs_frames());
    }
}
