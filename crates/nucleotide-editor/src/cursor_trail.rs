// ABOUTME: Neovide-style cursor trail: a single deformed quad whose four
// ABOUTME: corners are critically-damped springs, painted beneath the cursor
// ABOUTME: to draw out motion as a subtle smear.

//! Cursor trail animation, ported from Neovide's `cursor_renderer`.
//!
//! The editor's cursor is a rectangle. Instead of moving rigidly, each of the
//! four corners is a critically-damped spring targeting its own corner of the
//! cursor rect. Corners that face the direction of travel are given a shorter
//! spring (the "leading edge") and corners behind it a longer one (the
//! "trailing edge"), so the quad stretches into a short comet while the cursor
//! moves and collapses back to a clean rect when it stops.
//!
//! Two deliberate deviations from Neovide, both required by this codebase:
//!
//! * **Scroll decomposition.** Neovide animates its cursor toward
//!   scroll-affected destinations, which smears the quad on every scroll.
//!   Here, screen motion is split: grid motion (the cursor actually moving in
//!   the document) drives the springs; scroll motion is added rigidly to the
//!   corners (`current_position` *and* `previous_destination`) on every paint
//!   and never feeds the springs. This is the upstream #3543 correction that
//!   the reference fork reverted.
//!
//! * **Driver-side settling.** The trail integrates springs from the render
//!   driver (beside the scroll driver) using its own clock, and the paint
//!   path retargets/draws. This guarantees the frame loop terminates even when
//!   the cursor is not painted (it left the viewport), because `advance` can
//!   settle the springs without a paint.
//!
//! The trail state lives in an [`Rc`](std::rc::Rc) shared cell on
//! [`EditorViewState`](crate::view_state::EditorViewState). The cursor paint
//! path (`EditorCursor::paint`) reaches it through a thread-local ambient
//! scope that `view_component.rs` opens around the document paint call, since
//! the cursor painter is not given the view state.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Point, Size, Window, point, px, size};
use helix_view::graphics::CursorKind;

/// Default full-animation length in seconds.
///
/// Neovide defaults to 0.150 s. We deliberately pick a slightly tighter
/// 0.120 s so the trail reads as high-frequency motion blur rather than a
/// floating comet: the trailing corners are the ones that stretch, and
/// shortening them keeps the smear attached to the cursor movement instead of
/// drifting behind it on every keystroke.
const ANIMATION_LENGTH: f32 = 0.120;

/// Animation length used for hops of two cells or less (typing, held `j`).
///
/// Neovide uses 0.04 s; we use 0.035 s. Short hops repeat every keystroke, so
/// a fast spring keeps the cursor from lingering behind the typed character —
/// 0.035 s is about two frames at 60 Hz, which resolves as "the cursor snaps
/// onto the key you just pressed" while still letting the trailing edge smear
/// slightly.
const SHORT_ANIMATION_LENGTH: f32 = 0.035;

/// How far the quad is stretched by the moving cursor, 0..1.
///
/// `leading = animation_length * (1.0 - trail_size)`, so the scale runs
/// **backwards from intuition**: larger means *more* trail.
///
/// - `1.0` — `leading = 0`: the two leading corners reach the destination
///   instantly while the trailing corners take the full `animation_length`.
///   That is the maximum smear, and it is what Neovide ships.
/// - `0.0` — `leading = trailing = animation_length`: every corner moves
///   together, so there is no smear at all, just a smooth ease of the whole
///   cursor.
///
/// We sit just below the maximum, at 0.9, which gives the leading edge
/// `0.012 s` — about one frame at 60 Hz. Note that the restraint here is
/// about the *leading edge*, not about smear length: at 1.0 the front corners
/// teleport, which reads as the cursor detaching from the cell it just moved
/// onto, whereas a one-frame catch-up keeps the front attached to the
/// destination and lets the trailing corners supply the blur. Going the other
/// way, much below 0.9 lengthens the leading edge until the smear detaches
/// and reads as a ghost.
const TRAIL_SIZE: f32 = 0.9;

/// Horizontal short-hop threshold in cells, mirroring Neovide's 2.001.
const SHORT_JUMP_CELLS: f32 = 2.001;

/// Vertical short-hop tolerance in cells, mirroring Neovide's 0.001.
const SHORT_JUMP_VERTICAL_TOLERANCE: f32 = 0.001;

/// Fixed integration substep in seconds.
///
/// Neovide clamps its per-frame dt to `MAX_ANIMATION_DT = 1/120` and renders
/// at most once per frame. This editor can render several times per frame and
/// `advance` may be called from the render driver with arbitrary real elapsed
/// time, so we use the same 1/120 value as a fixed substep: each call
/// integrates the real elapsed time in ≤ 1/120 s chunks, so the discrete
/// spring matches Neovide's regardless of frame pacing.
const SUB_STEP_DT: f32 = 1.0 / 120.0;

/// How close to the destination a spring must come (pixels) before it snaps
/// shut. Same value as Neovide; it is the termination threshold the frame
/// loop relies on.
const SPRING_SNAP_DISTANCE: f32 = 0.01;

/// A critically damped spring for one axis of one corner.
///
/// Ported character-for-character from Neovide's
/// `CriticallyDampedSpringAnimation::update` (`renderer/animation_utils.rs`).
///
/// The spring holds the *error* — the distance between the corner's current
/// position and its destination — and `update` advances it toward zero. The
/// analytic formula for a critically damped harmonic oscillator is used:
///
/// ```text
/// zeta = 1.0
/// omega = 4.0 / (zeta * animation_length)      // 2%-settling in animation_length
/// a = position
/// b = position * omega + velocity
/// c = exp(-omega * dt)
/// position = (a + b * dt) * c
/// velocity = c * (-a * omega - b * dt * omega + b)
/// ```
///
/// When `|position| < 0.01` (or the animation length is shorter than `dt`)
/// the spring snaps: both position and velocity are zeroed, and `update`
/// reports that no further animation is needed. This is what terminates the
/// frame loop — see [`CursorTrail::needs_frames`].
#[derive(Debug, Clone, Copy)]
pub struct CriticallyDampedSpringAnimation {
    position: f32,
    velocity: f32,
}

impl CriticallyDampedSpringAnimation {
    fn new() -> Self {
        Self {
            position: 0.0,
            velocity: 0.0,
        }
    }

    /// Retargets the spring to a new error value.
    ///
    /// Mirrors Neovide's `Corner::update`, which sets `animation.position =
    /// delta` on a destination change *without* zeroing velocity. The carried
    /// velocity is what makes held motion (`j` in normal mode) flow smoothly:
    /// each per-frame retarget continues the previous frame's momentum
    /// instead of restarting from rest. Velocity is only discarded by
    /// [`Self::reset`] (snap / forced-immediate) or by the `|position| <
    /// 0.01` snap inside [`Self::update`].
    fn retarget(&mut self, position: f32) {
        self.position = position;
    }

    /// Advances the spring by `dt` seconds. Returns `false` once the spring
    /// is (or has become) settled, matching Neovide's return contract.
    fn update(&mut self, dt: f32, animation_length: f32) -> bool {
        if animation_length <= dt {
            self.reset();
            return false;
        }
        if self.position == 0.0 {
            return false;
        }

        // Simulate a critically damped spring, also known as a PD controller.
        // For more details of why this was chosen, see this:
        // https://gdcvault.com/play/1027059/Math-In-Game-Development-Summit
        // < 1 underdamped,  1 critically damped, > 1 overdamped
        let zeta = 1.0;
        // The omega is calculated so that the destination is reached with a
        // 2% tolerance in animation_length time.
        let omega = 4.0 / (zeta * animation_length);

        // Use the analytical formula for critically damped harmonic
        // oscillation. a and b are the initial conditions obtained by
        // setting dt to zero and solving for position and velocity
        // respectively.
        let a = self.position;
        let b = self.position * omega + self.velocity;

        let c = (-omega * dt).exp();

        self.position = (a + b * dt) * c;
        self.velocity = c * (-a * omega - b * dt * omega + b);

        if self.position.abs() < SPRING_SNAP_DISTANCE {
            self.reset();
            false
        } else {
            true
        }
    }

    /// Zeroes both position and velocity: the corner is at its destination
    /// and has no residual motion.
    fn reset(&mut self) {
        self.position = 0.0;
        self.velocity = 0.0;
    }

    fn position(&self) -> f32 {
        self.position
    }
}

/// One corner of the cursor quad.
#[derive(Debug, Clone, Copy)]
struct TrailCorner {
    /// Window-space position of this corner, in pixels. This is what gets
    /// drawn. It is maintained so that
    /// `current_position == corner_destination - spring_error` after every
    /// update, mirroring Neovide's `current_position = corner_destination -
    /// animation.position`.
    current_position: Point<f32>,
    /// Offset of this corner from the cursor rect origin, in pixels
    /// (TL `(0, 0)`, TR `(w, 0)`, BR `(w, h)`, BL `(0, h)`).
    relative_position: Point<f32>,
    /// The last destination this corner was retargeted to, in window space.
    /// Shifted rigidly with scroll so that scroll motion never looks like
    /// grid motion to the springs.
    previous_destination: Point<f32>,
    animation_x: CriticallyDampedSpringAnimation,
    animation_y: CriticallyDampedSpringAnimation,
    /// Per-corner animation length for the glide in flight. Assigned once
    /// when the destination changes (rank- and jump-dependent) and kept
    /// until the next retarget.
    animation_length: f32,
}

impl TrailCorner {
    fn new() -> Self {
        Self {
            current_position: point(0.0, 0.0),
            relative_position: point(0.0, 0.0),
            previous_destination: point(-1000.0, -1000.0),
            animation_x: CriticallyDampedSpringAnimation::new(),
            animation_y: CriticallyDampedSpringAnimation::new(),
            animation_length: 0.0,
        }
    }

    /// The window-space position this corner is targeting for a cursor rect
    /// whose top-left corner is at `origin` (window space, pixels).
    fn get_destination(&self, origin: Point<f32>) -> Point<f32> {
        add(origin, self.relative_position)
    }

    /// How aligned this corner is with the direction the cursor is moving,
    /// in `(-1, 1)`. Corners in front of the motion (positive alignment) are
    /// given the fast "leading" spring; corners behind it (negative) the slow
    /// "trailing" spring.
    ///
    /// Mirrors Neovide's `calculate_direction_alignment`: the corner's
    /// direction is measured from the cursor center to the corner, the travel
    /// direction from the corner's current position to its destination, and
    /// the alignment is their dot product.
    fn calculate_direction_alignment(&self, origin: Point<f32>, center: Point<f32>) -> f32 {
        let corner_destination = self.get_destination(origin);
        let corner_direction = normalize(sub(self.relative_position, center));
        let travel_direction = normalize(sub(corner_destination, self.current_position));
        travel_direction.x * corner_direction.x + travel_direction.y * corner_direction.y
    }
}

/// Immutable identity of the cursor shape being trailed. Any change snaps the
/// trail (forced-immediate reset), which covers mode changes (Block ↔ Bar),
/// focus changes (hollow block), and resize / font changes (width/height).
#[derive(Debug, Clone, Copy, PartialEq)]
struct CursorShapeIdentity {
    kind: CursorKind,
    hollow: bool,
    width: f32,
    height: f32,
}

/// The whole trail: four spring corners plus the paint-parity state the
/// springs need to stay consistent with the grid cursor.
pub struct CursorTrail {
    corners: [TrailCorner; 4],
    /// Size of the cursor rect currently being trailed, in pixels.
    rect_size: Size<f32>,
    /// Origin (top-left) of the cursor rect, in window space, for the frame
    /// currently being painted. Used to re-derive drawn positions from the
    /// springs after integration.
    rect_origin: Point<f32>,
    shape_identity: Option<CursorShapeIdentity>,
    /// Whether the current shape paints an outline (hollow block) instead of
    /// a filled rect.
    hollow: bool,
    /// Color the smear is drawn with. Not part of the identity: a color
    /// change mid-glide just repaints the in-flight quad.
    color: Hsla,
    /// Scroll position (window pixels) the corners were last consistent
    /// with. Each paint shifts the corners by the scroll delta.
    last_scroll: Point<f32>,
    /// Clock anchor for the spring integration. `Some` exactly while the
    /// trail is animating, so [`Self::needs_frames`] is the loop's liveness
    /// predicate.
    last_advance: Option<Instant>,
    enabled: bool,
    animation_length: f32,
    short_animation_length: f32,
    trail_size: f32,
}

impl Default for CursorTrail {
    fn default() -> Self {
        Self {
            corners: [TrailCorner::new(); 4],
            rect_size: size(0.0, 0.0),
            rect_origin: point(0.0, 0.0),
            shape_identity: None,
            hollow: false,
            color: Hsla::default(),
            last_scroll: point(0.0, 0.0),
            last_advance: None,
            enabled: true,
            animation_length: ANIMATION_LENGTH,
            short_animation_length: SHORT_ANIMATION_LENGTH,
            trail_size: TRAIL_SIZE,
        }
    }
}

impl CursorTrail {
    /// A trail with system defaults. The config lane can tune it later via
    /// the setters below.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.reset();
        }
        self.enabled = enabled;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Config-lane hook: full glide length in seconds.
    ///
    /// Clamped to `(0, 0.5]`: strictly positive, because `update`
    /// short-circuits on `animation_length <= dt` and snaps the spring, so a
    /// zero or negative value would silently kill the trail; and capped at
    /// 0.5 s so the smear cannot drift into a detached comet. Also re-clamps
    /// the short-hop length so the cross-field invariant `short <= animation`
    /// holds whichever setter runs last.
    pub fn set_animation_length(&mut self, seconds: f32) {
        self.animation_length = seconds.max(f32::EPSILON).min(0.5);
        self.short_animation_length = self.short_animation_length.min(self.animation_length);
    }

    /// Config-lane hook: short-hop length in seconds.
    ///
    /// Clamped to `(0, animation_length]`: strictly positive for the same
    /// reason as [`Self::set_animation_length`], and at most the full
    /// animation length so a single keystroke never animates slower than a
    /// long jump.
    pub fn set_short_animation_length(&mut self, seconds: f32) {
        self.short_animation_length = seconds.max(f32::EPSILON).min(self.animation_length);
    }

    /// Config-lane hook: trail stretch in `0..=1`; larger means more trail.
    ///
    /// `1.0` is the maximum smear (the leading corners snap to the
    /// destination while the trailing corners stretch the full length);
    /// `0.0` removes the smear entirely and just eases the whole cursor.
    /// Out-of-range values are clamped so the quad can never degenerate.
    pub fn set_trail_size(&mut self, trail_size: f32) {
        // `f32::clamp` propagates NaN, and TOML can spell one (`nan` and `inf`
        // are valid TOML floats), so a perfectly valid config file can hand us
        // a NaN here. It would flow into `leading = animation_length * (1.0 -
        // trail_size)` and from there into every corner coordinate and into
        // the path. Fall back to the default rather than to 0.0, so a typo
        // yields the standard trail instead of a mysteriously trail-less
        // cursor.
        self.trail_size = if trail_size.is_finite() {
            trail_size.clamp(0.0, 1.0)
        } else {
            TRAIL_SIZE
        };
    }

    /// Whether the frame loop must keep running.
    ///
    /// The loop's liveness is exactly "a clock has been started and not yet
    /// cleared": `last_advance` is `Some` while any spring is still moving
    /// and `None` once every spring has snapped to `|position| < 0.01`. The
    /// driver advances the trail only while this is true, and each advance
    /// integrates real elapsed time in fixed substeps, so a settled trail
    /// keeps `needs_frames() == false` no matter how many frames arrive — the
    /// loop cannot strand itself.
    pub fn needs_frames(&self) -> bool {
        self.last_advance.is_some()
    }

    /// Called by the render driver (beside the scroll driver) at the top of
    /// the render pass. Advances every spring by the real time elapsed since
    /// the previous advance/observe, in fixed ≤ 1/120 s substeps, using the
    /// shared `last_advance` clock. Safe to call even when the cursor is not
    /// painted that frame — this is what lets a trail settle (and the frame
    /// loop end) after the cursor leaves the viewport.
    pub fn advance(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        let Some(last) = self.last_advance else {
            // Not armed. The clock is started by the paint path when it
            // retargets; starting one here would turn a single unrelated
            // frame into a spurious animation loop.
            return;
        };
        let elapsed = now.saturating_duration_since(last);
        let animating = self.integrate(elapsed);
        self.last_advance = if animating { Some(now) } else { None };
    }

    /// Snaps the whole trail: springs are zeroed (position *and* velocity),
    /// corners are parked on the current destination, the shape identity is
    /// forgotten so the next paint re-installs cleanly, and any armed clock
    /// is cleared.
    ///
    /// Used for forced-immediate transitions (hidden cursor, cursor not
    /// painted, disabled), and deliberately *not* for shape changes — those
    /// go through [`Self::observe`]'s snap path which does the same zeroing
    /// while re-installing the new geometry.
    pub fn reset(&mut self) {
        for corner in &mut self.corners {
            corner.animation_x.reset();
            corner.animation_y.reset();
            corner.current_position = corner.previous_destination;
        }
        self.last_advance = None;
        self.shape_identity = None;
    }

    /// Paint-path entry point: observe the cursor rect that is about to be
    /// painted, animate the quad, and draw the smear beneath it.
    ///
    /// `rect` is the cursor rect in window space (scroll already applied by
    /// the painter). `kind`/`hollow` are the focus-mapped paint shape. `scroll`
    /// is the viewport's live scroll position in window pixels; `cell_width`
    /// and `line_height` are the grid cell dimensions used to normalize the
    /// short-hop check.
    pub fn paint_and_update(
        &mut self,
        rect: Bounds<Pixels>,
        kind: CursorKind,
        hollow: bool,
        color: Hsla,
        cell_width: Pixels,
        line_height: Pixels,
        scroll: Point<Pixels>,
        window: &mut Window,
    ) {
        if self.observe(
            rect,
            kind,
            hollow,
            color,
            cell_width,
            line_height,
            scroll,
            Instant::now(),
        ) {
            self.draw(window);
        }
    }

    /// The pure state half of [`Self::paint_and_update`], with an explicit
    /// clock so tests can drive time deterministically. Returns whether the
    /// quad is still animating and should be drawn.
    fn observe(
        &mut self,
        rect: Bounds<Pixels>,
        kind: CursorKind,
        hollow: bool,
        color: Hsla,
        cell_width: Pixels,
        line_height: Pixels,
        scroll: Point<Pixels>,
        now: Instant,
    ) -> bool {
        if !self.enabled {
            self.reset();
            return false;
        }

        let width = rect.size.width.as_f32();
        let height = rect.size.height.as_f32();
        if matches!(kind, CursorKind::Hidden) || width <= 0.0 || height <= 0.0 {
            // A hidden cursor (or a degenerate rect) paints nothing, so there
            // is nothing to trail: snap now so no stale smear is drawn when a
            // cursor reappears somewhere else.
            self.reset();
            return false;
        }

        let origin = point(rect.origin.x.as_f32(), rect.origin.y.as_f32());
        let identity = CursorShapeIdentity {
            kind,
            hollow,
            width,
            height,
        };

        if self.shape_identity != Some(identity) {
            // First paint, shape change (mode/focus/resize), or re-entry
            // after a reset. Forced-immediate: springs (position and
            // velocity) are zeroed and every corner is parked exactly on its
            // destination — None, no residual motion to animate. This is
            // upstream #3484: Neovide's fork only snaps `current_position`
            // here and leaves the springs armed, which makes the next real
            // move start from a stale velocity.
            self.snap_to(origin, width, height, hollow, scroll);
            self.shape_identity = Some(identity);
            self.color = color;
            return false;
        }

        self.color = color;
        self.rect_origin = origin;

        // --- Scroll decomposition -----------------------------------------
        // Everything below compares destinations in window space, which
        // scroll moves. Shift the corner state by the scroll delta first:
        // `current_position` and `previous_destination` both move rigidly
        // with the content. After the shift, "the destination changed" means
        // "the grid cursor moved", and only that drives the springs.
        let shift = point(
            self.last_scroll.x - scroll.x.as_f32(),
            self.last_scroll.y - scroll.y.as_f32(),
        );
        if shift.x != 0.0 || shift.y != 0.0 {
            for corner in &mut self.corners {
                corner.current_position = add(corner.current_position, shift);
                corner.previous_destination = add(corner.previous_destination, shift);
            }
        }
        self.last_scroll = point(scroll.x.as_f32(), scroll.y.as_f32());

        // --- Retarget on real grid motion --------------------------------
        let destination_changed = self.corners.iter().any(|corner| {
            corner.get_destination(origin) != corner.previous_destination
        });
        if destination_changed {
            self.retarget(origin, cell_width, line_height);
        }

        // --- Integrate the time the driver has not consumed yet -----------
        // The driver advances at the top of the render pass, so by the time
        // paint runs the frame's elapsed is usually already integrated and
        // `now - last_advance` is ~0. The same clock is shared, so elapsed is
        // never lost or double-counted across multiple renders per frame.
        let elapsed = self
            .last_advance
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        let animating = self.integrate(elapsed);
        self.last_advance = if animating { Some(now) } else { None };
        animating
    }

    /// Re-installs the corner geometry for a new cursor rect and parks every
    /// corner exactly on its destination. Called on first paint and on any
    /// shape-identity change, i.e. forced-immediate transitions: no spring
    /// position, no spring velocity, so nothing animates afterwards.
    fn snap_to(
        &mut self,
        origin: Point<f32>,
        width: f32,
        height: f32,
        hollow: bool,
        scroll: Point<Pixels>,
    ) {
        self.corners[0].relative_position = point(0.0, 0.0);
        self.corners[1].relative_position = point(width, 0.0);
        self.corners[2].relative_position = point(width, height);
        self.corners[3].relative_position = point(0.0, height);
        self.hollow = hollow;
        self.rect_size = size(width, height);
        self.rect_origin = origin;
        for corner in &mut self.corners {
            let destination = corner.get_destination(origin);
            corner.previous_destination = destination;
            corner.current_position = destination;
            corner.animation_x.reset();
            corner.animation_y.reset();
        }
        self.last_scroll = point(scroll.x.as_f32(), scroll.y.as_f32());
        self.last_advance = None;
    }

    /// Starts a new glide toward `origin` (used only when the destination
    /// actually changed). Assigns each corner its rank-dependent animation
    /// length — or the short-hop length when the move was two cells or
    /// less — then re-arms both springs with the current error.
    fn retarget(&mut self, origin: Point<f32>, cell_width: Pixels, line_height: Pixels) {
        let center = point(
            self.rect_size.width * 0.5,
            self.rect_size.height * 0.5,
        );

        // Rank the corners by direction alignment, least aligned (trailing)
        // first, ties broken by corner index — mirroring Neovide's sort in
        // `CursorRenderer::animate`.
        let mut ranked: Vec<(usize, f32)> = (0..4)
            .map(|id| (id, self.corners[id].calculate_direction_alignment(origin, center)))
            .collect();
        ranked.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        let leading = self.animation_length * (1.0 - self.trail_size).clamp(0.0, 1.0);
        let trailing = self.animation_length;

        let cell_w = cell_width.as_f32().max(f32::EPSILON);
        let cell_h = line_height.as_f32().max(f32::EPSILON);

        for (id, corner) in self.corners.iter_mut().enumerate() {
            let corner_destination = corner.get_destination(origin);
            // The jump vector is compared *after* the scroll shift, so it
            // measures grid motion in cell units. A short hop — typing or
            // holding `j` — animates with `min(animation_length,
            // short_animation_length)`, overriding the rank scheme.
            let jump_vec = point(
                (corner_destination.x - corner.previous_destination.x) / cell_w,
                (corner_destination.y - corner.previous_destination.y) / cell_h,
            );
            corner.animation_length = if jump_vec.x.abs() <= SHORT_JUMP_CELLS
                && jump_vec.y.abs() <= SHORT_JUMP_VERTICAL_TOLERANCE
            {
                self.animation_length.min(self.short_animation_length)
            } else {
                match ranked.iter().position(|(i, _)| *i == id).unwrap() {
                    // The leading edge runs faster than the trailing edge;
                    // with a trail size of one it jumps to the destination.
                    2..=3 => leading,
                    // One corner runs between the trailing corner and the
                    // leading edge, creating the triangular smear.
                    1 => (leading + trailing) / 2.0,
                    0 => trailing,
                    _ => unreachable!("a cursor quad has exactly four corners"),
                }
            };

            // Re-arm both springs with the current error. Velocity is kept
            // (Neovide behavior) so held motion flows instead of restarting.
            let delta = sub(corner_destination, corner.current_position);
            corner.animation_x.retarget(delta.x);
            corner.animation_y.retarget(delta.y);
            corner.previous_destination = corner_destination;
        }
    }

    /// Advances every corner by `elapsed` in fixed ≤ 1/120 s substeps, then
    /// re-derives the drawn positions from the springs (mirroring Neovide's
    /// `current_position = corner_destination - animation.position`).
    ///
    /// Returns whether any spring is still moving (|error| ≥ 0.01 px).
    fn integrate(&mut self, elapsed: Duration) -> bool {
        let mut total = elapsed.as_secs_f32();
        while total > 0.0 {
            let dt = total.min(SUB_STEP_DT);
            for corner in &mut self.corners {
                let length = corner.animation_length;
                let _ = corner.animation_x.update(dt, length);
                let _ = corner.animation_y.update(dt, length);
            }
            total -= dt;
        }

        let origin = self.rect_origin;
        for corner in &mut self.corners {
            let destination = corner.get_destination(origin);
            corner.current_position = point(
                destination.x - corner.animation_x.position(),
                destination.y - corner.animation_y.position(),
            );
        }

        self.corners.iter().any(|corner| {
            corner.animation_x.position().abs() >= SPRING_SNAP_DISTANCE
                || corner.animation_y.position().abs() >= SPRING_SNAP_DISTANCE
        })
    }

    /// Draws the quad as a single filled polygon (or a stroked outline for a
    /// hollow cursor) beneath the cursor rect. Neovide's per-corner gradient
    /// (`smear_gradient`) is not reproduced: `paint_path` fills one path with
    /// one color and has no per-vertex alpha, so the gradient is unreachable
    /// without splitting the quad into N sub-polygons — not justified for a
    /// sub-100ms smear.
    fn draw(&self, window: &mut Window) {
        let corner = |i: usize| {
            point(
                px(self.corners[i].current_position.x.round()),
                px(self.corners[i].current_position.y.round()),
            )
        };
        let mut builder = if self.hollow {
            PathBuilder::stroke(px(1.0))
        } else {
            PathBuilder::fill()
        };
        builder.add_polygon(&[corner(0), corner(1), corner(2), corner(3)], true);
        if let Ok(path) = builder.build() {
            window.paint_path(path, self.color);
        }
    }
}

/// Ambient-paint plumbing.
///
/// `EditorCursor::paint` (in `cursor.rs`) has no access to the view state, so
/// the document paint closure in `view_component.rs` opens a scope around the
/// whole document paint carrying `(trail, scroll, cell_width)`, and the
/// cursor's paint step reads the innermost scope. Nested scopes (multiple
/// editor views in one window) shadow correctly because paints never
/// interleave.

struct ActiveCursorTrailScope {
    trail: Rc<RefCell<CursorTrail>>,
    scroll: Point<Pixels>,
    cell_width: Pixels,
}

thread_local! {
    static ACTIVE_CURSOR_TRAIL_SCOPES: RefCell<Vec<ActiveCursorTrailScope>> =
        const { RefCell::new(Vec::new()) };
}

/// RAII guard that pops the ambient scope when dropped.
pub struct CursorTrailScopeGuard;

/// Opens an ambient cursor-trail scope for the duration of the document paint
/// call. Returns a guard; the scope ends when the guard is dropped.
pub fn enter(
    trail: Rc<RefCell<CursorTrail>>,
    scroll: Point<Pixels>,
    cell_width: Pixels,
) -> CursorTrailScopeGuard {
    ACTIVE_CURSOR_TRAIL_SCOPES.with(|stack| {
        stack
            .borrow_mut()
            .push(ActiveCursorTrailScope { trail, scroll, cell_width });
    });
    CursorTrailScopeGuard
}

impl Drop for CursorTrailScopeGuard {
    fn drop(&mut self) {
        ACTIVE_CURSOR_TRAIL_SCOPES.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

/// Runs `f` with the innermost active scope, if any. Returns `None` when the
/// cursor is being painted outside the document-paint harness (then the trail
/// simply isn't drawn).
pub fn with_active_scope<R>(
    f: impl FnOnce(&Rc<RefCell<CursorTrail>>, Point<Pixels>, Pixels) -> R,
) -> Option<R> {
    ACTIVE_CURSOR_TRAIL_SCOPES.with(|stack| {
        let stack = stack.borrow();
        stack
            .last()
            .map(|scope| f(&scope.trail, scope.scroll, scope.cell_width))
    })
}

fn add(a: Point<f32>, b: Point<f32>) -> Point<f32> {
    point(a.x + b.x, a.y + b.y)
}

fn sub(a: Point<f32>, b: Point<f32>) -> Point<f32> {
    point(a.x - b.x, a.y - b.y)
}

fn normalize(v: Point<f32>) -> Point<f32> {
    let len = (v.x * v.x + v.y * v.y).sqrt();
    if len > f32::EPSILON {
        point(v.x / len, v.y / len)
    } else {
        point(0.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL_WIDTH: Pixels = px(8.0);
    const LINE_HEIGHT: Pixels = px(20.0);

    fn screen(x: f32, y: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(8.0), px(20.0)),
        }
    }

    fn no_scroll() -> Point<Pixels> {
        point(px(0.0), px(0.0))
    }

    fn color() -> Hsla {
        Hsla {
            h: 0.6,
            s: 0.8,
            l: 0.5,
            a: 1.0,
        }
    }

    fn observe_block(
        trail: &mut CursorTrail,
        rect: Bounds<Pixels>,
        scroll: Point<Pixels>,
        now: Instant,
    ) -> bool {
        trail.observe(rect, CursorKind::Block, false, color(), CELL_WIDTH, LINE_HEIGHT, scroll, now)
    }

    /// Installs a fresh trail on a block cursor at `(100, 100)` and returns
    /// it with the clock anchored at `t0`.
    fn installed_trail(t0: Instant) -> CursorTrail {
        let mut trail = CursorTrail::new();
        // First observe has no prior identity: it snaps, does not animate.
        assert!(!observe_block(&mut trail, screen(100.0, 100.0), no_scroll(), t0));
        trail
    }

    // ------------------------------------------------------------------
    // Spring mechanics
    // ------------------------------------------------------------------

    #[test]
    fn spring_advances_with_the_analytic_critical_damping_formula() {
        // Expected value is derived from the spring's defining formula
        // (the requirement, ported character-for-character from Neovide):
        //
        //   omega = 4 / animation_length = 4 / 0.120 = 33.3333 s⁻¹
        //   a = position = 100.0
        //   b = position * omega + velocity = 100 * 33.3333 + 0 = 3333.33
        //   c = exp(-omega * dt) = exp(-33.3333 * (1/120)) = exp(-0.277778) = 0.757468
        //   position' = (a + b * dt) * c
        //             = (100.0 + 3333.33 * 0.00833333) * 0.757468
        //             = 127.778 * 0.757468
        //             = 96.789
        //
        // With a velocity of zero at rest, the spring must decay toward 0
        // without overshooting (critical damping) and must still report
        // "animating" (position ≥ 0.01).
        let mut spring = CriticallyDampedSpringAnimation::new();
        spring.retarget(100.0);
        assert!(spring.update(1.0 / 120.0, 0.120));
        let expected = (100.0_f32 + 100.0_f32 * (4.0_f32 / 0.120_f32) * (1.0_f32 / 120.0_f32))
            * (-(4.0_f32 / 0.120_f32) * (1.0_f32 / 120.0_f32)).exp();
        assert!(
            (spring.position - expected).abs() < 1e-2,
            "position {} vs derived {}",
            spring.position,
            expected
        );
    }

    #[test]
    fn spring_snaps_below_one_hundredth_and_zeroes_velocity() {
        // Requirement: when a substep lands the spring inside the snap
        // threshold, BOTH position and velocity go to zero and update()
        // reports "settled". Zeroing only the position would leave momentum
        // behind for the next move to inherit — the same class of bug as
        // upstream neovide #3484, where a forced-immediate move snapped the
        // position but left the springs alive.
        //
        // Setup derivation. The threshold is checked AFTER the integration
        // step, not before (neovide animation_utils.rs:117), so the snap fires
        // only when the position *produced by this substep* is under 0.01:
        //
        //   omega = 4 / 0.120         = 33.3333
        //   c     = exp(-omega / 120) = 0.757466
        //   a     = 0.005
        //   b     = a * omega + v     = 0.16667 + v
        //   p'    = (a + b * dt) * c  = (0.005 + (0.16667 + v) * 0.0083333) * 0.757466
        //
        // p' < 0.01 requires v < 0.817. An earlier version of this test used
        // v = 5.0 and asserted the snap fired; it does not — that velocity
        // pushes p' to 0.0364, above the threshold, so the step correctly
        // reports "still animating". That test asserted a pre-integration
        // threshold check the formula does not have; the implementation was
        // right. v = 0.5 sits well inside the bound while staying large enough
        // that a leaked velocity is visible below.
        let mut spring = CriticallyDampedSpringAnimation::new();
        spring.retarget(0.005);
        spring.velocity = 0.5;
        assert!(!spring.update(1.0 / 120.0, 0.120), "snap must fire");
        assert_eq!(spring.position, 0.0);
        assert_eq!(spring.velocity, 0.0);

        // The consequence that actually matters: a later move must not inherit
        // the momentum the snap discarded. A spring that snapped and a spring
        // that never moved have to produce the same first substep. A leaked
        // velocity of v separates them by exactly v * dt * c = 0.5 * 0.0083333
        // * 0.757466 ~= 0.0032, comfortably outside the 1e-4 tolerance.
        let mut from_rest = CriticallyDampedSpringAnimation::new();
        spring.retarget(100.0);
        from_rest.retarget(100.0);
        assert!(spring.update(1.0 / 120.0, 0.120));
        assert!(from_rest.update(1.0 / 120.0, 0.120));
        assert!(
            (spring.position - from_rest.position).abs() < 1e-4,
            "snapped spring inherited momentum: {} vs from-rest {}",
            spring.position,
            from_rest.position
        );
    }

    #[test]
    fn spring_resets_when_animation_length_is_shorter_than_dt() {
        // Requirement: `animation_length <= dt` is a hard reset path — the
        // spring must not attempt a step it cannot resolve.
        let mut spring = CriticallyDampedSpringAnimation::new();
        spring.retarget(100.0);
        assert!(!spring.update(0.02, 0.01));
        assert_eq!(spring.position, 0.0);
        assert_eq!(spring.velocity, 0.0);
    }

    // ------------------------------------------------------------------
    // Rank ordering: trailing lags, leading leads
    // ------------------------------------------------------------------

    #[test]
    fn horizontal_travel_assigns_rank_lengths_and_trailing_corner_lags() {
        // The cursor hops 3 cells right (24 px), which is longer than the
        // short-hop threshold, so rank lengths apply.
        //
        // Travel direction is (1, 0). Corner directions measured from the
        // rect center (w=8, h=20) are:
        //   TL = normalize(-4, -10) = (-0.371, -0.928)
        //   TR = normalize( 4, -10) = ( 0.371, -0.928)
        //   BR = normalize( 4,  10) = ( 0.371,  0.928)
        //   BL = normalize(-4,  10) = (-0.371,  0.928)
        // Alignment = travel · corner direction:
        //   TL = -0.371, BL = -0.371, TR = +0.371, BR = +0.371.
        // Sorted ascending, ties by corner index: TL (rank 0, trailing),
        // BL (rank 1), TR (rank 2, leading), BR (rank 3, leading).
        //
        // Requirement: rank 2..=3 → leading = 0.120 * (1 - 0.9) = 0.012;
        // rank 1 → (0.012 + 0.120)/2 = 0.066; rank 0 → trailing = 0.120.
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        observe_block(&mut trail, screen(124.0, 100.0), no_scroll(), t0);

        assert!((trail.corners[0].animation_length - 0.120).abs() < 1e-6, "TL trailing");
        assert!((trail.corners[3].animation_length - 0.066).abs() < 1e-6, "BL rank 1");
        assert!((trail.corners[1].animation_length - 0.012).abs() < 1e-6, "TR leading");
        assert!((trail.corners[2].animation_length - 0.012).abs() < 1e-6, "BR leading");

        // After one substep, the trailing corner must have decayed less than
        // the leading corner. The per-step decay factor is
        // (1 + omega*dt) * exp(-omega*dt), which decreases monotonically as
        // omega*dt grows; omega = 4/length, so the trailing corner (length
        // 0.120, omega = 33.3) decays slower than the leading corner
        // (length 0.012, omega = 333.3). Each spring starts from the same
        // error delta, so |TL position| > |TR position| after any number of
        // identical substeps.
        let t1 = t0 + Duration::from_secs_f32(1.0 / 120.0);
        trail.advance(t1);
        let tl_error = trail.corners[0].animation_x.position().abs();
        let tr_error = trail.corners[1].animation_x.position().abs();
        assert!(
            tl_error > tr_error,
            "trailing corner ({tl_error}) must lag behind leading corner ({tr_error})"
        );
    }

    #[test]
    fn vertical_travel_assigns_rank_lengths_with_bottom_corners_leading() {
        // The cursor hops 3 cells down (60 px). Travel direction is (0, 1).
        // Alignments (travel · corner direction, using the same directions as
        // the horizontal test):
        //   TL = -0.928, TR = -0.928, BR = +0.928, BL = +0.928.
        // Sorted ascending, ties by index: TL (rank 0, trailing), TR (rank 1),
        // BR (rank 2, leading), BL (rank 3, leading).
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        observe_block(&mut trail, screen(100.0, 160.0), no_scroll(), t0);

        assert!((trail.corners[0].animation_length - 0.120).abs() < 1e-6, "TL trailing");
        assert!((trail.corners[1].animation_length - 0.066).abs() < 1e-6, "TR rank 1");
        assert!((trail.corners[2].animation_length - 0.012).abs() < 1e-6, "BR leading");
        assert!((trail.corners[3].animation_length - 0.012).abs() < 1e-6, "BL leading");
    }

    // ------------------------------------------------------------------
    // Short-hop suppression
    // ------------------------------------------------------------------

    #[test]
    fn hops_of_two_cells_or_less_use_the_short_animation_length() {
        // cell_width = 8 px, so a 1-cell right hop is 8 px and a 2-cell hop
        // is 16 px. With the scroll-shifted previous_destination, the jump
        // vector is (pixels / cell size):
        //   8/8 = 1.0 ≤ 2.001 and 16/8 = 2.0 ≤ 2.001, |y| = 0 ≤ 0.001 → short.
        // Requirement: every corner — regardless of rank — animates with
        // min(animation_length, short_animation_length) = min(0.120, 0.035) =
        // 0.035 for the whole hop.
        for hop in [8.0, 16.0] {
            let t0 = Instant::now();
            let mut trail = installed_trail(t0);
            assert!(
                observe_block(&mut trail, screen(100.0 + hop, 100.0), no_scroll(), t0),
                "a short hop must arm the springs"
            );
            for corner in &trail.corners {
                assert!(
                    (corner.animation_length - 0.035).abs() < 1e-6,
                    "corner length {} should be the short length",
                    corner.animation_length
                );
            }
        }
    }

    #[test]
    fn three_cell_hop_or_vertical_motion_bypasses_short_animation() {
        // 24 px / 8 px = 3.0 cells > 2.001 → not short, rank lengths apply
        // (TL trailing = 0.120).
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        observe_block(&mut trail, screen(124.0, 100.0), no_scroll(), t0);
        assert!((trail.corners[0].animation_length - 0.120).abs() < 1e-6);

        // A 1-cell vertical hop: dx = 0, dy = 20/20 = 1.0 cell, which exceeds
        // the 0.001 vertical tolerance → not short.
        let mut vertical = installed_trail(t0);
        observe_block(&mut vertical, screen(100.0, 120.0), no_scroll(), t0);
        assert!((vertical.corners[0].animation_length - 0.120).abs() < 1e-6);

        // A diagonal hop (1 cell right, 1 cell down) also has |dy| = 1.0 >
        // 0.001 → not short.
        let mut diagonal = installed_trail(t0);
        observe_block(&mut diagonal, screen(108.0, 120.0), no_scroll(), t0);
        assert!((diagonal.corners[0].animation_length - 0.120).abs() < 1e-6);
    }

    // ------------------------------------------------------------------
    // Scroll decomposition
    // ------------------------------------------------------------------

    #[test]
    fn page_down_with_stationary_grid_cursor_produces_zero_smear() {
        // Scroll the content down 5 rows (5 × 20 = 100 px). The grid cursor
        // does not move; it is simply painted 100 px higher on screen: rect
        // (100, 100) → (100, 0) with scroll (0, 100).
        //
        // The scroll shift moves current_position and previous_destination by
        // (0, -100), so the shifted previous destination equals the new
        // destination exactly → no retarget, springs stay at zero, and the
        // observe reports "settled". The quad is exactly the cursor rect:
        // zero smear, proof that scroll motion never feeds the springs.
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        assert!(!observe_block(
            &mut trail,
            screen(100.0, 0.0),
            point(px(0.0), px(100.0)),
            t0,
        ));

        for corner in &trail.corners {
            let destination = corner.get_destination(trail.rect_origin);
            assert_eq!(corner.current_position, destination);
            assert_eq!(corner.animation_x.position(), 0.0);
            assert_eq!(corner.animation_y.position(), 0.0);
        }
        assert!(!trail.needs_frames(), "a scroll-only frame must not arm the loop");
    }

    #[test]
    fn scroll_only_observe_does_not_change_inflight_springs() {
        // Arm a 1-cell right hop, then advance one substep so the spring is
        // mid-flight. Then paint a frame where ONLY the scroll changed (down
        // 50 px): the rect origin moves (108, 100) → (108, 50) with scroll
        // (0, 50). The shift moves current/previous in lockstep, no retarget
        // fires, and the frame's elapsed is zero (same instant), so the
        // spring error must be bit-identical before and after the scroll
        // frame. This is the #3543 essence: scrolling is rigid, not animated.
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        assert!(observe_block(&mut trail, screen(108.0, 100.0), no_scroll(), t0));

        let t1 = t0 + Duration::from_secs_f32(1.0 / 120.0);
        trail.advance(t1);
        let error_before = trail.corners[0].animation_x.position();

        assert!(
            observe_block(
                &mut trail,
                screen(108.0, 50.0),
                point(px(0.0), px(50.0)),
                t1,
            ),
            "the glide must still be in flight after a scroll-only frame"
        );
        assert_eq!(
            trail.corners[0].animation_x.position(),
            error_before,
            "scroll motion must not perturb in-flight springs"
        );
    }

    // ------------------------------------------------------------------
    // Forced-immediate motion
    // ------------------------------------------------------------------

    #[test]
    fn shape_change_zeroes_spring_velocity_and_position() {
        // Start a glide, then change shape mid-flight (Block → Bar), which
        // happens on a mode change. The identity (kind, hollow, width,
        // height) differs, so the trail must snap: springs zeroed in position
        // AND velocity, corners parked exactly on the new destination, no
        // animation left.
        //
        // This deliberately asserts the velocity is zero: the reference fork
        // (mod.rs:141-144) only snaps `current_position` on immediate
        // movement and leaves the springs armed, so the next real move starts
        // from stale momentum. Upstream #3484 fixes it; this test pins that.
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        observe_block(&mut trail, screen(108.0, 100.0), no_scroll(), t0);

        let bar_rect = Bounds {
            origin: point(px(108.0), px(100.0)),
            size: size(px(2.0), px(20.0)),
        };
        assert!(!trail.observe(
            bar_rect,
            CursorKind::Bar,
            false,
            color(),
            CELL_WIDTH,
            LINE_HEIGHT,
            no_scroll(),
            t0,
        ));

        for corner in &trail.corners {
            assert_eq!(corner.animation_x.position(), 0.0);
            assert_eq!(corner.animation_y.position(), 0.0);
            assert_eq!(corner.animation_x.velocity, 0.0);
            assert_eq!(corner.animation_y.velocity, 0.0);
            assert_eq!(
                corner.current_position,
                corner.get_destination(trail.rect_origin),
                "corners must sit exactly on the new shape's destination"
            );
        }
        assert!(!trail.needs_frames());
    }

    #[test]
    fn hollow_toggle_also_snaps() {
        // Focus change paints the same Block rect hollow; identity changes
        // (hollow flipped) → forced-immediate snap.
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        let hollow = screen(100.0, 100.0);
        assert!(!trail.observe(
            hollow,
            CursorKind::Block,
            true,
            color(),
            CELL_WIDTH,
            LINE_HEIGHT,
            no_scroll(),
            t0,
        ));
        for corner in &trail.corners {
            assert_eq!(corner.animation_x.velocity, 0.0);
            assert_eq!(corner.animation_y.velocity, 0.0);
        }
    }

    #[test]
    fn hidden_cursor_resets_the_trail() {
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        observe_block(&mut trail, screen(108.0, 100.0), no_scroll(), t0);

        assert!(!trail.observe(
            screen(108.0, 100.0),
            CursorKind::Hidden,
            false,
            color(),
            CELL_WIDTH,
            LINE_HEIGHT,
            no_scroll(),
            t0,
        ));
        assert!(!trail.needs_frames());
        // Forgetting the identity means the next visible paint snaps instead
        // of animating from a stale ghost position.
        let mut trail2 = trail;
        assert!(!observe_block(&mut trail2, screen(108.0, 100.0), no_scroll(), t0));
        assert!(!trail2.needs_frames());
    }

    // ------------------------------------------------------------------
    // Frame-loop liveness and termination
    // ------------------------------------------------------------------

    #[test]
    fn needs_frames_lifecycle_tracks_arming_and_settling() {
        let t0 = Instant::now();
        let mut trail = CursorTrail::new();
        assert!(!trail.needs_frames(), "a fresh trail is idle");

        assert!(!observe_block(&mut trail, screen(100.0, 100.0), no_scroll(), t0));
        assert!(!trail.needs_frames(), "parking on a fresh identity is not motion");

        assert!(observe_block(&mut trail, screen(108.0, 100.0), no_scroll(), t0));
        assert!(trail.needs_frames(), "armed by the hop");

        // An identical, unmoving observe in mid-flight keeps the loop alive…
        assert!(observe_block(&mut trail, screen(108.0, 100.0), no_scroll(), t0));
        assert!(trail.needs_frames());

        // …but once the springs settle, the same observe must report idle
        // (elapsed zero, springs snapped in a previous advance).
        let mut t = t0;
        for _ in 0..128 {
            t += Duration::from_secs_f32(1.0 / 120.0);
            trail.advance(t);
        }
        assert!(!trail.needs_frames());
        assert!(!observe_block(&mut trail, screen(108.0, 100.0), no_scroll(), t));
        assert!(!trail.needs_frames(), "a settled trail must stay idle");
    }

    #[test]
    fn frame_loop_terminates_within_a_bounded_number_of_advances() {
        // A 1-cell hop arms the springs at 8 px with the short length
        // (0.035 s) → ω = 4/0.035 ≈ 114.3 s⁻¹. Starting at position 8 with
        // zero velocity, each 1/120 s substep decays position by at most
        // (1 + ω·dt)·exp(-ω·dt) = (1 + 0.952)·exp(-0.952) ≈ 0.753 (velocity
        // terms only speed up the decay for critically damped flow), so the
        // number of steps to fall below 0.01 is bounded by
        //   ln(8 / 0.01) / ln(1 / 0.753) ≈ 6.68 / 0.284 ≈ 24
        // steps. Thus a generous 64-step bound proves the loop terminates;
        // after settling, further advances must not re-arm it.
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        observe_block(&mut trail, screen(108.0, 100.0), no_scroll(), t0);
        assert!(trail.needs_frames());

        let mut t = t0;
        let mut steps = 0;
        while trail.needs_frames() && steps < 64 {
            t += Duration::from_secs_f32(1.0 / 120.0);
            trail.advance(t);
            steps += 1;
        }
        assert!(!trail.needs_frames(), "loop must terminate (took {steps} steps)");
        assert!(steps <= 64, "settled in {steps} steps");
        for corner in &trail.corners {
            assert_eq!(
                corner.current_position,
                corner.get_destination(trail.rect_origin),
                "settled corners sit exactly on their destinations"
            );
        }

        // Settled stays settled: more advances change nothing and do not
        // restart the clock.
        for _ in 0..8 {
            t += Duration::from_secs_f32(1.0 / 120.0);
            trail.advance(t);
        }
        assert!(!trail.needs_frames());
    }

    #[test]
    fn advance_before_arming_does_not_start_the_clock() {
        // `advance` on an idle trail must not plant a `last_advance` clock —
        // that would keep `needs_frames()` true forever on the next scroll
        // animation.
        let t0 = Instant::now();
        let mut trail = CursorTrail::new();
        trail.advance(t0);
        assert!(!trail.needs_frames());
        trail.advance(t0 + Duration::from_secs_f32(0.1));
        assert!(!trail.needs_frames());
    }

    #[test]
    fn reset_clears_armed_state_and_forgets_identity() {
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        observe_block(&mut trail, screen(108.0, 100.0), no_scroll(), t0);
        assert!(trail.needs_frames());

        trail.reset();
        assert!(!trail.needs_frames());
        // Identity forgotten → the next observe parks instead of animating.
        assert!(!observe_block(&mut trail, screen(108.0, 100.0), no_scroll(), t0));
        assert!(!trail.needs_frames());
    }

    #[test]
    fn disabling_resets_and_never_animates() {
        let t0 = Instant::now();
        let mut trail = installed_trail(t0);
        trail.set_enabled(false);
        assert!(!trail.needs_frames());
        assert!(!observe_block(&mut trail, screen(200.0, 200.0), no_scroll(), t0));
        assert!(!trail.needs_frames());
        for corner in &trail.corners {
            assert_eq!(corner.animation_x.position(), 0.0);
            assert_eq!(corner.animation_y.position(), 0.0);
        }

        // Re-enabling parks on the next paint (identity was forgotten).
        trail.set_enabled(true);
        assert!(!observe_block(&mut trail, screen(200.0, 200.0), no_scroll(), t0));
        assert!(!trail.needs_frames());
    }

    // ------------------------------------------------------------------
    // Ambient scope
    // ------------------------------------------------------------------

    #[test]
    fn ambient_scope_round_trips_trail_scroll_and_cell_width() {
        let trail = Rc::new(RefCell::new(CursorTrail::new()));
        let scroll = point(px(3.0), px(4.0));
        let guard = enter(trail.clone(), scroll, px(9.0));

        with_active_scope(|active, active_scroll, cell_width| {
            assert!(Rc::ptr_eq(active, &trail));
            assert_eq!(active_scroll, scroll);
            assert_eq!(cell_width, px(9.0));
        })
        .expect("scope must be visible inside paint");

        drop(guard);
        assert!(
            with_active_scope(|_, _, _| ()).is_none(),
            "scope must be gone after the guard drops"
        );
    }

    #[test]
    fn nested_scopes_shadow_outer_scopes() {
        let outer = Rc::new(RefCell::new(CursorTrail::new()));
        let inner = Rc::new(RefCell::new(CursorTrail::new()));

        let outer_guard = enter(outer.clone(), point(px(0.0), px(0.0)), px(8.0));
        let inner_guard = enter(inner.clone(), point(px(1.0), px(2.0)), px(9.0));

        with_active_scope(|active, scroll, cell_width| {
            assert!(Rc::ptr_eq(active, &inner), "innermost scope wins");
            assert_eq!(scroll, point(px(1.0), px(2.0)));
            assert_eq!(cell_width, px(9.0));
        })
        .expect("inner scope visible");

        drop(inner_guard);
        with_active_scope(|active, _, _| {
            assert!(Rc::ptr_eq(active, &outer), "outer scope visible again");
        })
        .expect("outer scope visible");
        drop(outer_guard);
    }
}