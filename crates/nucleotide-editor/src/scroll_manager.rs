// ABOUTME: Native pixel scroll state for the GPUI editor viewport
// ABOUTME: Tracks visual-row positions and sub-row offsets for smooth scrolling

use gpui::{Pixels, Point, Size, point, px, size};
use nucleotide_logging::trace;
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use crate::scroll_animation::ScrollAnimation;

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
    /// Returns whether the offset changed and how many whole document lines the
    /// scroll position crossed. Wheel scrolling uses this to keep fractional
    /// pixel movement local while letting the GUI viewport decide when to sync
    /// the visible visual row back to Helix.
    pub(crate) fn scroll_by_delta(&self, delta: Point<Pixels>) -> (bool, isize) {
        // A wheel gesture is a continuous user intent; it overrides an in-flight
        // discrete tween rather than fighting it.
        self.animation.cancel("wheel_scroll");
        let old_position = self.scroll_position.get();
        let old_line = self.pixels_to_anchor(old_position.y);
        let next_offset = self.scroll_offset() + delta;

        self.set_scroll_offset_internal(next_offset, false, "wheel_scroll");

        let new_position = self.scroll_position.get();
        let new_line = self.pixels_to_anchor(new_position.y);
        let crossed_lines = new_line as isize - old_line as isize;
        if crossed_lines != 0 || old_position.x != new_position.x {
            self.pending_view_sync.set(true);
        }

        (old_position != new_position, crossed_lines)
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
    }

    pub(crate) fn scroll_animation_enabled(&self) -> bool {
        self.animation.enabled()
    }

    /// Whether the editor render pass has to keep requesting frames.
    ///
    /// This is deliberately a predicate of *desired* motion rather than of
    /// per-frame movement: a frame that produces no pixel change must still
    /// schedule the next one, otherwise a tween stalls and never completes.
    /// Currently it covers the discrete scroll tween; a later phase extends it
    /// to also cover a wheel momentum arming window, which is why it is its own
    /// named predicate rather than an inlined `is_active()`.
    pub(crate) fn scroll_needs_frames(&self) -> bool {
        self.animation.is_active()
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
        let clamped = self.clamp_position(target);
        let from = self.scroll_position.get().y;
        if clamped.y == from {
            self.animation.cancel("scroll_target_already_reached");
            return;
        }

        self.animation.animate_to(from, clamped.y, duration);
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

    #[test]
    fn test_scroll_by_delta_cancels_in_flight_scroll_animation() {
        let manager = scrollable_manager();
        manager.set_scroll_animation_enabled(true);
        manager.animate_scroll_to(point(px(0.0), px(400.0)), Duration::from_millis(200));
        assert!(manager.scroll_animation_active());

        let (changed, crossed_lines) = manager.scroll_by_delta(point(px(0.0), px(-5.0)));

        assert!(changed);
        assert_eq!(crossed_lines, 0);
        assert!(!manager.scroll_animation_active());
        assert_eq!(manager.scroll_position(), point(px(0.0), px(5.0)));
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
