// ABOUTME: Time-based scroll animation for smooth viewport transitions
// ABOUTME: Animates pixel scroll position toward targets without breaking Helix sync

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{Pixels, px};
use nucleotide_logging::trace;

/// Tween duration for a single visual row jump.
///
/// 60ms rather than the earlier 80ms looks like a regression for a one-row move
/// and is not: perceived arrival got *longer*. Quint at 86ms is perceptually
/// complete at ~38ms, quad at 66ms at ~59ms. A one-row move only travels 20px, so
/// what matters is that every frame still changes something — quad's last 5ms
/// moves 2px where quint's last 45ms moved 0.2px.
const BASE_JUMP_DURATION_MS: f32 = 60.0;
/// Extra duration granted per visual row of distance.
const JUMP_DURATION_MS_PER_ROW: f32 = 6.0;
/// Hard upper bound on any tween, so even a huge jump settles quickly.
///
/// This is the *only* clamp in [`editor_jump_duration`]. It is reached at
/// `(280 - 60) / 6 = 36.67` rows, i.e. 36 rows is the last value that still
/// grows. A second, lower clamp inside the formula would silently shadow this
/// bound and make the documented maximum unreachable.
const MAX_JUMP_DURATION_MS: f32 = 280.0;

/// Ease-out quadratic, `2t - t²`. Strictly monotonic on [0,1], bounded by 1, and its
/// velocity `2(1-t)` reaches exactly 0 at t=1 — a smooth stop with no overshoot.
/// Perceptually ~90% complete at the end of its nominal duration, so the tail is
/// visible motion rather than dead time.
fn ease_out_quad(t: f32) -> f32 {
    t * (2.0 - t)
}

#[derive(Debug, Clone)]
struct AnimState {
    from: Pixels,
    to: Pixels,
    start: Instant,
    duration: Duration,
    /// Stored as a plain function pointer so the curve is resolved once per
    /// animation rather than rebuilt on every frame.
    easing: fn(f32) -> f32,
}

/// A single in-flight scroll tween, or nothing at all.
///
/// Sampling is a pure function of the elapsed time, never an incremental step.
/// `NativeEditorView` is `RenderOnce` and a frame can run `render` more than
/// once, so an incremental `pos += velocity` design would double-advance.
#[derive(Debug, Clone)]
pub(crate) struct ScrollAnimation {
    state: Rc<RefCell<Option<AnimState>>>,
    enabled: Rc<Cell<bool>>,
}

/// Duration used for a discrete scroll of `rows` visual rows.
///
/// Linear in the absolute row distance, bounded above by [`MAX_JUMP_DURATION_MS`]
/// and by nothing else. There is deliberately no inner row clamp: an earlier
/// version clamped rows to 24 here, which capped the real maximum at 224ms and
/// left the declared 240ms limit permanently unreachable — the bound that is
/// documented is the bound that has to be in force.
pub(crate) fn editor_jump_duration(rows: isize) -> Duration {
    let millis = (BASE_JUMP_DURATION_MS + JUMP_DURATION_MS_PER_ROW * rows.unsigned_abs() as f32)
        .min(MAX_JUMP_DURATION_MS);
    Duration::from_millis(millis.round() as u64)
}

impl Default for ScrollAnimation {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollAnimation {
    /// Smooth scrolling is opt-in: a fresh animation never moves anything, so
    /// every existing instant-scroll caller keeps its exact behavior.
    pub(crate) fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(None)),
            enabled: Rc::new(Cell::new(false)),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.set(enabled);
        if !enabled {
            self.cancel("smooth_scrolling_disabled");
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.get()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.state.borrow().is_some()
    }

    /// Drop the in-flight tween, if there is one.
    ///
    /// `reason` names the *cause* of the drop rather than the function that
    /// performed it, so drops from one cause aggregate onto a single trace line
    /// and genuinely different causes stay separable in the log.
    ///
    /// Only a drop that actually killed a running tween is logged. Cancels are
    /// issued constantly while idle, and logging those would bury the one signal
    /// this exists to produce, so a no-op cancel returns without a trace.
    pub(crate) fn cancel(&self, reason: &'static str) {
        let killed = self.state.borrow_mut().take();
        let Some(anim) = killed else {
            return;
        };
        trace!(
            reason,
            from = ?anim.from,
            to = ?anim.to,
            duration_ms = ?(anim.duration.as_secs_f32() * 1000.0),
            "ScrollAnimation tween cancelled"
        );
    }

    /// Start a tween from `from` to `to` using the current clock.
    pub(crate) fn animate_to(&self, from: Pixels, to: Pixels, duration: Duration) {
        self.animate_to_at(from, to, duration, Instant::now());
    }

    /// Start a tween from `from` to `to` with an injected start instant.
    pub(crate) fn animate_to_at(
        &self,
        from: Pixels,
        to: Pixels,
        duration: Duration,
        start: Instant,
    ) {
        if !self.enabled.get() {
            self.cancel("animation_disabled");
            return;
        }

        // A tween armed while another is still in flight silently replaces it.
        // That is itself a plausible way for a tween to die before its
        // destination, so the replacement is recorded rather than being treated
        // as an internal detail.
        let replaced = self.state.borrow().clone();

        // A zero-length tween would divide by zero below; 1ms keeps the math
        // finite and lands it on the next sample.
        let duration = if duration.is_zero() {
            Duration::from_millis(1)
        } else {
            duration
        };

        *self.state.borrow_mut() = Some(AnimState {
            from,
            to,
            start,
            duration,
            easing: ease_out_quad,
        });

        trace!(
            ?from,
            ?to,
            duration_ms = ?(duration.as_secs_f32() * 1000.0),
            replaced_tween = replaced.is_some(),
            replaced_from = ?replaced.as_ref().map(|previous| previous.from),
            replaced_to = ?replaced.as_ref().map(|previous| previous.to),
            "ScrollAnimation tween armed"
        );
    }

    /// Sample the tween at `now`. `None` when idle.
    pub(crate) fn advance(&self, now: Instant) -> Option<Pixels> {
        let anim = self.state.borrow().clone()?;

        let elapsed = now.saturating_duration_since(anim.start);
        let t = if anim.duration.is_zero() {
            1.0
        } else {
            (elapsed.as_secs_f32() / anim.duration.as_secs_f32()).clamp(0.0, 1.0)
        };

        if t >= 1.0 {
            trace!(
                elapsed_ms = ?(elapsed.as_secs_f32() * 1000.0),
                ?t,
                from = ?anim.from,
                to = ?anim.to,
                position = ?anim.to,
                completed = true,
                "ScrollAnimation tween sampled"
            );
            self.cancel("tween_completed");
            return Some(anim.to);
        }

        let eased = (anim.easing)(t);
        let position = anim.from + (anim.to - anim.from) * eased;
        trace!(
            elapsed_ms = ?(elapsed.as_secs_f32() * 1000.0),
            ?t,
            from = ?anim.from,
            to = ?anim.to,
            position = ?position,
            completed = false,
            "ScrollAnimation tween sampled"
        );
        Some(position)
    }

    /// Re-anchor a running tween onto the live scroll position, keeping the
    /// destination. Used when the line height or content extent changes while a
    /// tween is in flight, which would otherwise ease from a stale origin.
    ///
    /// Re-anchoring must never *extend* total flight time: the remaining budget
    /// is carried over instead of restarting the original duration. Otherwise a
    /// caller that re-anchors every painted frame could keep resetting the
    /// clock and the tween would never advance past `t = 0`.
    pub(crate) fn retarget_from(&self, current: Pixels) {
        let mut state = self.state.borrow_mut();
        let Some(anim) = state.as_mut() else {
            return;
        };
        let now = Instant::now();
        let remaining = anim
            .duration
            .saturating_sub(now.saturating_duration_since(anim.start))
            // A zero remainder would complete on the very next sample; keep one
            // frame of travel so the tween still reads as motion.
            .max(Duration::from_millis(16));
        let previous_from = anim.from;
        anim.from = current;
        anim.start = now;
        anim.duration = remaining;
        // Re-anchoring restarts the clock, so a caller that re-anchors every
        // painted frame can hold a tween near its origin indefinitely. Logged
        // because "ran short" and "kept getting re-anchored" look identical from
        // the sampler log alone.
        trace!(
            previous_from = ?previous_from,
            from = ?anim.from,
            to = ?anim.to,
            remaining_ms = ?(remaining.as_secs_f32() * 1000.0),
            "ScrollAnimation tween re-anchored"
        );
    }

    /// Keep the destination inside the scrollable range, dropping the tween when
    /// clamping collapses it onto the current position.
    pub(crate) fn clamp_target(&self, max_y: Pixels) {
        let mut state = self.state.borrow_mut();
        let Some(anim) = state.as_mut() else {
            return;
        };
        anim.to = anim.to.max(px(0.0)).min(max_y);
        if anim.to == anim.from {
            // This is a kill that does not go through `cancel`, so it would
            // otherwise be invisible in the log.
            trace!(
                reason = "clamp_target_no_room",
                from = ?anim.from,
                to = ?anim.to,
                max_y = ?max_y,
                "ScrollAnimation tween cancelled"
            );
            *state = None;
        }
    }

    /// The destination while a tween is in flight.
    pub(crate) fn destination(&self) -> Option<Pixels> {
        self.state.borrow().as_ref().map(|anim| anim.to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_jump_duration_grows_with_distance_and_stays_capped() {
        assert_eq!(editor_jump_duration(0), Duration::from_millis(60));
        assert_eq!(editor_jump_duration(1), Duration::from_millis(66));
        assert_eq!(editor_jump_duration(-1), Duration::from_millis(66));
        assert_eq!(editor_jump_duration(20), Duration::from_millis(180));
        // 36 rows is the last value that still grows: 60 + 6 * 36 = 276.
        assert_eq!(editor_jump_duration(36), Duration::from_millis(276));
        // 40 rows is past the knee: 60 + 6 * 40 = 300, held down to the cap.
        assert_eq!(editor_jump_duration(40), Duration::from_millis(280));
        assert_eq!(editor_jump_duration(-40), Duration::from_millis(280));
    }

    /// Regression test for the unreachable-cap bug.
    ///
    /// The old formula clamped rows internally to 24, so the real top out was
    /// 224ms and the declared 240ms maximum was never in force — the cap test
    /// passed for the wrong reason. If a second, lower clamp ever reappears in
    /// the formula this fails, because the cap stops being the binding limit.
    #[test]
    fn editor_jump_duration_cap_is_actually_reachable() {
        // (280 - 60) / 6 = 36.67, so 37 is the first row count that saturates.
        assert_eq!(editor_jump_duration(36), Duration::from_millis(276));
        assert_eq!(editor_jump_duration(37), Duration::from_millis(280));
        assert_eq!(editor_jump_duration(-37), Duration::from_millis(280));

        // A saturating row count must land on the cap, not blow past it or
        // saturate the f32 multiply.
        assert_eq!(editor_jump_duration(1_000_000), Duration::from_millis(280));
        assert_eq!(editor_jump_duration(isize::MAX), Duration::from_millis(280));
        assert_eq!(editor_jump_duration(isize::MIN), Duration::from_millis(280));

        // Everything past the knee is flat at the cap.
        for rows in 37..=1_000isize {
            assert_eq!(editor_jump_duration(rows), Duration::from_millis(280));
        }
    }

    #[test]
    fn ease_out_quad_hits_its_endpoints_exactly() {
        assert_eq!(ease_out_quad(0.0), 0.0);
        assert_eq!(ease_out_quad(1.0), 1.0);
    }

    #[test]
    fn ease_out_quad_is_monotonic_and_never_overshoots() {
        let mut previous = f32::NEG_INFINITY;
        for step in 0..=1_000 {
            let t = step as f32 / 1_000.0;
            let eased = ease_out_quad(t);
            assert!(
                eased > previous,
                "ease_out_quad({t}) = {eased} did not advance past {previous}"
            );
            assert!(
                (0.0..=1.0).contains(&eased),
                "ease_out_quad({t}) = {eased} left [0, 1]"
            );
            previous = eased;
        }
    }

    #[test]
    fn ease_out_quad_spot_values() {
        assert!(
            (ease_out_quad(0.5) - 0.75).abs() < 1e-6,
            "f(0.5) = {}",
            ease_out_quad(0.5)
        );
        assert!(
            (ease_out_quad(0.9) - 0.99).abs() < 1e-6,
            "f(0.9) = {}",
            ease_out_quad(0.9)
        );
    }

    /// The reason for the swap. A quint ease is perceptually complete at ~44% of
    /// its nominal duration, so well over half of every tween showed a motionless
    /// screen. Quad keeps moving through its tail: the last fifth of the flight
    /// still covers 4% of the distance, roughly a hundred times what quint
    /// managed over the same window.
    #[test]
    fn ease_out_quad_tail_still_moves() {
        let tail = 1.0 - ease_out_quad(0.8);
        assert!(
            tail >= 0.03,
            "the last 20% of the flight only moved {tail} of the distance"
        );
    }

    /// Proves the tween samples the swapped curve, not merely that the curve
    /// function behaves: a 200ms 0->200 tween sampled at t=0.5 must read 150px,
    /// because quad(0.5) is 0.75. Quint would have read 193.75 here.
    #[test]
    fn advance_samples_the_quad_curve() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        let start = Instant::now();
        animation.animate_to_at(px(0.0), px(200.0), Duration::from_millis(200), start);

        let mid = animation
            .advance(start + Duration::from_millis(100))
            .expect("mid-flight sample");

        assert!(
            (mid - px(150.0)).abs() < px(0.5),
            "halfway sample {mid:?} is not the quad midpoint"
        );
        assert!(animation.is_active());
    }

    #[test]
    fn editor_jump_duration_is_monotonic_in_absolute_rows() {
        let mut previous = Duration::ZERO;
        for rows in 0..=64isize {
            let duration = editor_jump_duration(rows);
            assert!(
                duration >= previous,
                "duration for {rows} rows ({duration:?}) shrank from {previous:?}"
            );
            assert_eq!(duration, editor_jump_duration(-rows));
            previous = duration;
        }
    }

    #[test]
    fn animation_is_disabled_by_default() {
        let animation = ScrollAnimation::new();
        assert!(!animation.enabled());
        assert!(!animation.is_active());

        animation.animate_to(px(0.0), px(50.0), Duration::from_millis(100));

        assert!(!animation.is_active());
        assert_eq!(animation.destination(), None);
        assert_eq!(animation.advance(Instant::now()), None);
    }

    #[test]
    fn disabling_cancels_an_in_flight_animation() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        assert!(animation.enabled());
        animation.animate_to(px(0.0), px(50.0), Duration::from_millis(100));
        assert!(animation.is_active());

        animation.set_enabled(false);

        assert!(!animation.is_active());
        assert_eq!(animation.advance(Instant::now()), None);
    }

    #[test]
    fn advance_midflight_value_is_between_from_and_to() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        let start = Instant::now();
        animation.animate_to_at(px(0.0), px(200.0), Duration::from_millis(200), start);

        let mid = animation
            .advance(start + Duration::from_millis(100))
            .expect("mid-flight sample");

        assert!(mid > px(0.0), "mid-flight sample {mid:?} did not move");
        assert!(mid < px(200.0), "mid-flight sample {mid:?} overshot");
        assert!(animation.is_active());
        assert_eq!(animation.destination(), Some(px(200.0)));
    }

    #[test]
    fn advance_is_idempotent_for_the_same_instant() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        let start = Instant::now();
        animation.animate_to_at(px(0.0), px(200.0), Duration::from_millis(200), start);
        let sample = start + Duration::from_millis(60);

        let first = animation.advance(sample);
        let second = animation.advance(sample);
        let third = animation.advance(sample);

        assert_eq!(first, second);
        assert_eq!(second, third);
        assert!(first.is_some());
    }

    #[test]
    fn advance_at_duration_lands_exactly_on_target_and_deactivates() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        let start = Instant::now();
        animation.animate_to_at(px(10.0), px(200.0), Duration::from_millis(200), start);

        let landed = animation.advance(start + Duration::from_millis(200));

        assert_eq!(landed, Some(px(200.0)));
        assert!(!animation.is_active());
        assert_eq!(animation.advance(start + Duration::from_millis(200)), None);
        // Overshooting the deadline is still the destination.
        assert_eq!(animation.destination(), None);
    }

    #[test]
    fn zero_duration_tween_settles_on_the_next_sample() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        let start = Instant::now();
        animation.animate_to_at(px(0.0), px(10.0), Duration::ZERO, start);

        assert!(animation.is_active());
        assert_eq!(
            animation.advance(start + Duration::from_millis(1)),
            Some(px(10.0))
        );
        assert!(!animation.is_active());
    }

    #[test]
    fn retarget_from_reanchors_onto_the_current_position() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        animation.animate_to(px(0.0), px(200.0), Duration::from_millis(200));

        animation.retarget_from(px(50.0));

        assert_eq!(animation.destination(), Some(px(200.0)));
        // Re-anchoring restarts the clock, so the immediate sample sits on the
        // new origin instead of jumping ahead on the old one.
        let resumed = animation.advance(Instant::now()).expect("resumed sample");
        assert!(
            resumed >= px(50.0),
            "resumed sample {resumed:?} before the origin"
        );
        assert!(
            resumed < px(200.0),
            "resumed sample {resumed:?} already at target"
        );
    }

    #[test]
    fn retarget_from_is_a_noop_while_idle() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);

        animation.retarget_from(px(50.0));

        assert!(!animation.is_active());
        assert_eq!(animation.destination(), None);
    }

    #[test]
    fn retarget_from_preserves_remaining_flight_time() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        // Start with a deadline that has already passed, so the remaining budget
        // is known to be exactly zero and nothing here depends on wall-clock
        // timing between statements.
        let start = Instant::now() - Duration::from_millis(300);
        animation.animate_to_at(px(0.0), px(200.0), Duration::from_millis(200), start);
        assert!(animation.is_active());

        animation.retarget_from(px(100.0));

        // The original budget was already spent, so re-anchoring can only carry
        // over the one-frame floor. Restarting the full 200ms would hand out a
        // second full-duration flight and the tween would still be mid-air at
        // the 20ms sample below.
        assert_eq!(
            animation.advance(Instant::now() + Duration::from_millis(20)),
            Some(px(200.0))
        );
        assert!(!animation.is_active());
    }

    #[test]
    fn repeated_retarget_from_does_not_extend_the_tween_deadline() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        // Half the budget is already spent, so the true deadline is ~100ms out.
        animation.animate_to_at(
            px(0.0),
            px(200.0),
            Duration::from_millis(200),
            Instant::now() - Duration::from_millis(100),
        );
        let start = Instant::now();

        // Five re-anchors, as a per-frame caller would do. Each one hands the
        // *remaining* budget forward rather than a fresh 200ms, so the deadline
        // stays at ~100ms instead of moving out to 200ms on the last one.
        animation.retarget_from(px(20.0));
        animation.retarget_from(px(40.0));
        animation.retarget_from(px(60.0));
        animation.retarget_from(px(80.0));
        animation.retarget_from(px(100.0));

        // 120ms is past the preserved deadline but comfortably short of a
        // restarted 200ms flight.
        assert_eq!(
            animation.advance(start + Duration::from_millis(120)),
            Some(px(200.0))
        );
        assert!(!animation.is_active());
    }

    #[test]
    fn clamp_target_keeps_the_destination_inside_the_scrollable_range() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        animation.animate_to(px(100.0), px(500.0), Duration::from_millis(200));

        animation.clamp_target(px(200.0));

        assert_eq!(animation.destination(), Some(px(200.0)));
        assert!(animation.is_active());
    }

    #[test]
    fn clamp_target_drops_an_animation_with_no_room_left() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        animation.animate_to(px(0.0), px(500.0), Duration::from_millis(200));

        animation.clamp_target(px(0.0));

        assert!(!animation.is_active());
        assert_eq!(animation.destination(), None);
    }

    #[test]
    fn cancel_drops_the_in_flight_animation() {
        let animation = ScrollAnimation::new();
        animation.set_enabled(true);
        animation.animate_to(px(0.0), px(200.0), Duration::from_millis(200));

        animation.cancel("test");

        assert!(!animation.is_active());
        assert_eq!(animation.advance(Instant::now()), None);
    }
}
