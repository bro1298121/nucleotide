// ABOUTME: Native GPUI editor view component shell
// ABOUTME: Composes editor document painting with viewport input and scrollbars

use std::{rc::Rc, time::Instant};

use gpui::{
    App, Bounds, Component, EntityId, FocusHandle, Hsla, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Pixels, RenderOnce, Size, Styled as _, TextStyle, Window, div,
};

use crate::{
    CursorOverlayPlan, EditorDocumentElement, EditorLayout, EditorScrollbarMarker, EditorSurface,
    EditorSurfacePointerEvent, EditorViewState, EditorViewport, ViewportScrollUpdate,
    cursor_trail,
    selection::EditorPointerSelectionPhase,
};

type ScrollCallback = Rc<dyn Fn(&EditorViewport, ViewportScrollUpdate, &mut App)>;
type PointerCallback = Rc<dyn Fn(EditorSurfacePointerEvent, &mut App)>;
type PointerSelectionCallback =
    Rc<dyn Fn(EditorPointerSelectionPhase, EditorSurfacePointerEvent, &mut App) -> bool>;
type CursorOverlayCallback = Rc<dyn Fn(Option<CursorOverlayPlan>, &mut App)>;
type KeyDownCallback = Rc<dyn Fn(&KeyDownEvent, &mut Window, &mut App) -> bool>;

#[derive(Debug, Clone, Copy, PartialEq)]
struct EditorSurfaceRerenderSnapshot {
    gutter_width: Pixels,
    viewport_size: Size<Pixels>,
    max_scroll_offset: gpui::Size<Pixels>,
    /// Whether the paint currently being bracketed armed scroll motion.
    ///
    /// A per-paint *level* copied from the view state, but acted on as a
    /// transition. A paint that only *finishes* an armed tween also reports
    /// `true` — the level is rewritten on every layout sync — and scheduling
    /// another frame for that would run the frame loop past its own deadline.
    reveal_armed_motion: bool,
}

impl EditorSurfaceRerenderSnapshot {
    fn from_state(state: &EditorViewState) -> Self {
        Self {
            gutter_width: state.layout_snapshot().gutter_width,
            viewport_size: state.viewport().viewport_bounds().size,
            max_scroll_offset: state.viewport().max_scroll_offset(),
            reveal_armed_motion: state.cursor_reveal_armed_motion(),
        }
    }

    fn requires_rerender_after(self, next: Self) -> bool {
        self.gutter_width != next.gutter_width
            || self.viewport_size != next.viewport_size
            || self.max_scroll_offset != next.max_scroll_offset
    }

    /// Whether this paint turned scroll motion *on*.
    ///
    /// Split out from [`Self::requires_rerender_after`] because the two have
    /// different reasons to act: one means the surface has to be rebuilt, the
    /// other means the frame loop has to be restarted.
    fn armed_scroll_motion(self, next: Self) -> bool {
        !self.reveal_armed_motion && next.reveal_armed_motion
    }
}

pub struct NativeEditorView<P> {
    view_entity_id: EntityId,
    editor_state: EditorViewState,
    text_style: TextStyle,
    paint: P,
    focus: Option<FocusHandle>,
    scrollbar_thumb_color: Option<Hsla>,
    scrollbar_markers: Vec<EditorScrollbarMarker>,
    on_scroll: Option<ScrollCallback>,
    on_key_down: Option<KeyDownCallback>,
    on_cursor_overlay: Option<CursorOverlayCallback>,
    on_pointer_selection: Option<PointerSelectionCallback>,
    on_mouse_down: Option<PointerCallback>,
    on_mouse_drag: Option<PointerCallback>,
    on_mouse_up: Option<PointerCallback>,
}

impl<P> NativeEditorView<P>
where
    P: FnMut(
            &mut EditorViewState,
            Bounds<Pixels>,
            &mut EditorLayout,
            &mut Window,
            &mut App,
        ) -> Option<CursorOverlayPlan>
        + 'static,
{
    pub fn new(
        view_entity_id: EntityId,
        editor_state: EditorViewState,
        text_style: TextStyle,
        paint: P,
    ) -> Self {
        Self {
            view_entity_id,
            editor_state,
            text_style,
            paint,
            focus: None,
            scrollbar_thumb_color: None,
            scrollbar_markers: Vec::new(),
            on_scroll: None,
            on_key_down: None,
            on_cursor_overlay: None,
            on_pointer_selection: None,
            on_mouse_down: None,
            on_mouse_drag: None,
            on_mouse_up: None,
        }
    }

    pub fn scrollbar_thumb_color(mut self, color: Hsla) -> Self {
        self.scrollbar_thumb_color = Some(color);
        self
    }

    pub fn scrollbar_markers(mut self, markers: Vec<EditorScrollbarMarker>) -> Self {
        self.scrollbar_markers = markers;
        self
    }

    pub fn track_focus(mut self, focus: FocusHandle) -> Self {
        self.focus = Some(focus);
        self
    }

    pub fn on_key_down(
        mut self,
        callback: impl Fn(&KeyDownEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.on_key_down = Some(Rc::new(callback));
        self
    }

    pub fn on_scroll(
        mut self,
        callback: impl Fn(&EditorViewport, ViewportScrollUpdate, &mut App) + 'static,
    ) -> Self {
        self.on_scroll = Some(Rc::new(callback));
        self
    }

    pub fn on_cursor_overlay(
        mut self,
        callback: impl Fn(Option<CursorOverlayPlan>, &mut App) + 'static,
    ) -> Self {
        self.on_cursor_overlay = Some(Rc::new(callback));
        self
    }

    pub fn on_pointer_selection(
        mut self,
        callback: impl Fn(EditorPointerSelectionPhase, EditorSurfacePointerEvent, &mut App) -> bool
        + 'static,
    ) -> Self {
        self.on_pointer_selection = Some(Rc::new(callback));
        self
    }

    pub fn on_mouse_down(
        mut self,
        callback: impl Fn(EditorSurfacePointerEvent, &mut App) + 'static,
    ) -> Self {
        self.on_mouse_down = Some(Rc::new(callback));
        self
    }

    pub fn on_mouse_drag(
        mut self,
        callback: impl Fn(EditorSurfacePointerEvent, &mut App) + 'static,
    ) -> Self {
        self.on_mouse_drag = Some(Rc::new(callback));
        self
    }

    pub fn on_mouse_up(
        mut self,
        callback: impl Fn(EditorSurfacePointerEvent, &mut App) + 'static,
    ) -> Self {
        self.on_mouse_up = Some(Rc::new(callback));
        self
    }
}

impl<P> IntoElement for NativeEditorView<P>
where
    P: FnMut(
            &mut EditorViewState,
            Bounds<Pixels>,
            &mut EditorLayout,
            &mut Window,
            &mut App,
        ) -> Option<CursorOverlayPlan>
        + 'static,
{
    type Element = Component<Self>;

    fn into_element(self) -> Self::Element {
        Component::new(self)
    }
}

impl<P> RenderOnce for NativeEditorView<P>
where
    P: FnMut(
            &mut EditorViewState,
            Bounds<Pixels>,
            &mut EditorLayout,
            &mut Window,
            &mut App,
        ) -> Option<CursorOverlayPlan>
        + 'static,
{
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let NativeEditorView {
            view_entity_id,
            editor_state,
            text_style,
            mut paint,
            focus,
            scrollbar_thumb_color,
            scrollbar_markers,
            on_scroll,
            on_key_down,
            on_cursor_overlay,
            on_pointer_selection,
            on_mouse_down,
            on_mouse_drag,
            on_mouse_up,
        } = self;

        // Drive the scroll motion from the render pass. Requesting a frame
        // notifies this view, which renders again and advances the animation, so
        // the loop is self-sustaining the same way `AnimationElement` drives
        // itself.
        //
        // Liveness is keyed on `scroll_needs_frames()`, never on whether this
        // frame happened to move any pixels: a frame with no movement must still
        // schedule the next one, otherwise a tween stalls mid-flight and never
        // completes. `cx.notify` is the load-bearing tick; `request_animation_frame`
        // is only vsync pacing, so it is safe to request unconditionally.
        //
        // All of the scroll work happens inside this one `if`: the in-flight
        // tween (a discrete jump, a wheel glide, or the cursor-follow carry) is
        // sampled first, and only then are the pending gestures given the chance
        // to arm. That order matters — a new tween's target is the position left
        // on screen by this frame, and `advance_scrolloff_inertia` refuses to arm
        // while anything is in flight. The same `if` is what keeps the frame loop
        // from ever becoming a second loop: the loop runs exactly as long as
        // `scroll_needs_frames()` says it should, and all three terms of that
        // predicate are deadlines.
        //
        // The cursor trail rides the same loop, next to the scroll driver. Its
        // liveness is exactly `last_advance.is_some()` (`needs_frames()`): the
        // clock is started by the paint path when a retarget arms the springs
        // and cleared when every spring snaps (`|position| < 0.01`). Advancing
        // it here, *after* the scroll terms, lets a trail settle on frames
        // where the cursor is not painted without ever feeding scroll motion
        // into the springs — scroll is applied rigidly to the trail's positions
        // at observe time, and the two loops share only the notify/raf tick.
        let scroll_needs_frames = editor_state.viewport().scroll_needs_frames();
        let trail = editor_state.cursor_trail();
        let trail_needs_frames = trail.borrow().needs_frames();
        if scroll_needs_frames || trail_needs_frames {
            if scroll_needs_frames {
                editor_state.viewport().advance_scroll_animation();
                editor_state.viewport().advance_wheel_glide(Instant::now());
                editor_state.viewport().advance_scrolloff_inertia(Instant::now());
            }
            if trail_needs_frames {
                trail.borrow_mut().advance(Instant::now());
            }
            cx.notify(view_entity_id);
            window.request_animation_frame();
        }

        let root = div().id("editor-content").w_full().h_full().flex();

        let viewport = editor_state.viewport().clone();
        let surface_metrics = editor_state.surface_metrics().clone();
        let vertical_scrollbar_state = editor_state.vertical_scrollbar_state().clone();
        let horizontal_scrollbar_state = editor_state.horizontal_scrollbar_state().clone();
        let mut paint_editor_state = editor_state;
        let document_element =
            EditorDocumentElement::new(text_style, move |bounds, after_layout, window, cx| {
                let rerender_snapshot_before =
                    EditorSurfaceRerenderSnapshot::from_state(&paint_editor_state);

                // Cursor trail ambience. The cursor painter draws the smear
                // beneath the cursor rect, but it has no channel for the live
                // scroll position or the grid cell width — those live in the
                // view state, and scroll is applied by the caret painter at
                // paint time. Push an ambient scope (trail + scroll + cell
                // width) for the duration of this paint; `EditorCursor::paint`
                // reads it and drives the trail's observe/draw. The scope is
                // dropped immediately after paint so no stale trail ever bleeds
                // into a later paint in the same frame.
                let trail = paint_editor_state.cursor_trail();
                let trail_needs_frames_before = trail.borrow().needs_frames();
                let trail_scroll = paint_editor_state.viewport().scroll_position();
                let trail_cell_width = paint_editor_state.surface_metrics().get().cell_width;
                let trail_scope =
                    cursor_trail::enter(trail.clone(), trail_scroll, trail_cell_width);
                let overlay_plan = paint(&mut paint_editor_state, bounds, after_layout, window, cx);
                drop(trail_scope);

                if overlay_plan.is_none() {
                    // No cursor painted this frame (hidden, unfocused overlay,
                    // etc.). Zero the trail and forget its shape identity so the
                    // next visible cursor snaps in instead of dragging a smear
                    // from the last position it was drawn at.
                    trail.borrow_mut().reset();
                }

                let rerender_snapshot_after =
                    EditorSurfaceRerenderSnapshot::from_state(&paint_editor_state);
                if rerender_snapshot_before.requires_rerender_after(rerender_snapshot_after) {
                    // Paint runs after the surface has already rendered from
                    // the old viewport. Schedule the owning view to render
                    // again after this frame unwinds.
                    cx.defer(move |cx| {
                        cx.notify(view_entity_id);
                    });
                }

                if rerender_snapshot_before.armed_scroll_motion(rerender_snapshot_after) {
                    // The frame driver at the top of `render` consults
                    // `scroll_needs_frames()` *before* paint runs, and this
                    // paint is where a cursor reveal is applied. On the frame
                    // that arms a `Scrolloff` tween the predicate was already
                    // false and the `request_animation_frame()` inside that
                    // `if` never ran, so the tween would be armed with no frame
                    // left to advance it and the view would sit still for good.
                    //
                    // `cx.notify` is the whole of the fix. It marks the view
                    // dirty, GPUI re-renders it, and that render re-enters the
                    // driver — where `scroll_needs_frames()` is now true and the
                    // driver requests the animation frame itself.
                    //
                    // It must NOT also call `request_animation_frame` from here.
                    // This defer runs inside `flush_effects`, outside the
                    // `with_rendered_view` scope, and `request_animation_frame`
                    // reaches `Window::current_view`, which unwraps
                    // `rendered_entity_stack.last()` (gpui `window.rs:4315`).
                    // That stack is empty at this point, so calling it from a
                    // defer panics with "called `Option::unwrap()` on a `None`
                    // value" the first time a cursor reveal eases. The driver's
                    // own call is safe because it happens during `render`, with
                    // the stack populated.
                    cx.defer(move |cx| {
                        cx.notify(view_entity_id);
                    });
                }

                if !trail_needs_frames_before && trail.borrow().needs_frames() {
                    // Exactly the same trap as `armed_scroll_motion` above,
                    // for the cursor trail: the frame driver consulted
                    // `needs_frames()` before this paint ran, so on the first
                    // frame this paint retargets a spring (the trail's clock
                    // starts here, not in the driver) the `request_animation_frame`
                    // inside the driver's `if` has already been decided. A bare
                    // `cx.notify` restarts the loop and the next render's driver
                    // sees `needs_frames() == true` and requests the frame itself.
                    // Same rule applies: no `request_animation_frame` from inside
                    // a defer (gpui `window.rs:4315` panics).
                    cx.defer(move |cx| {
                        cx.notify(view_entity_id);
                    });
                }

                if let Some(on_cursor_overlay) = &on_cursor_overlay {
                    on_cursor_overlay(overlay_plan, cx);
                }
            });

        let mut editor_surface = EditorSurface::new(
            view_entity_id,
            viewport,
            surface_metrics,
            vertical_scrollbar_state,
            horizontal_scrollbar_state,
            document_element,
        );

        if let Some(scrollbar_thumb_color) = scrollbar_thumb_color {
            editor_surface = editor_surface.scrollbar_thumb_color(scrollbar_thumb_color);
        }
        editor_surface = editor_surface.scrollbar_markers(scrollbar_markers);

        if let Some(on_scroll) = on_scroll {
            editor_surface = editor_surface.on_scroll(move |viewport, update, cx| {
                on_scroll(viewport, update, cx);
            });
        }

        if let Some(focus) = focus {
            editor_surface = editor_surface.track_focus(focus);
        }

        if let Some(on_key_down) = on_key_down {
            editor_surface =
                editor_surface.on_key_down(move |event, window, cx| on_key_down(event, window, cx));
        }

        if on_pointer_selection.is_some() || on_mouse_down.is_some() {
            let on_pointer_selection = on_pointer_selection.clone();
            editor_surface = editor_surface.on_mouse_down(move |event, cx| {
                let mut changed = false;
                if let Some(on_pointer_selection) = &on_pointer_selection {
                    changed |= on_pointer_selection(EditorPointerSelectionPhase::Begin, event, cx);
                }
                if let Some(on_mouse_down) = &on_mouse_down {
                    on_mouse_down(event, cx);
                    changed = true;
                }
                changed
            });
        }

        if on_pointer_selection.is_some() || on_mouse_drag.is_some() {
            let on_pointer_selection = on_pointer_selection.clone();
            editor_surface = editor_surface.on_mouse_drag(move |event, cx| {
                let mut changed = false;
                if let Some(on_pointer_selection) = &on_pointer_selection {
                    changed |= on_pointer_selection(EditorPointerSelectionPhase::Extend, event, cx);
                }
                if let Some(on_mouse_drag) = &on_mouse_drag {
                    on_mouse_drag(event, cx);
                    changed = true;
                }
                changed
            });
        }

        if on_pointer_selection.is_some() || on_mouse_up.is_some() {
            editor_surface = editor_surface.on_mouse_up(move |event, cx| {
                let mut changed = false;
                if let Some(on_pointer_selection) = &on_pointer_selection {
                    changed |= on_pointer_selection(EditorPointerSelectionPhase::End, event, cx);
                }
                if let Some(on_mouse_up) = &on_mouse_up {
                    on_mouse_up(event, cx);
                    changed = true;
                }
                changed
            });
        }

        let paint_area = div().id("editor-paint-area").w_full().h_full().flex_1();

        root.child(paint_area.child(editor_surface))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use gpui::{
        AppContext as _, Empty, Entity, FocusHandle, Keystroke, MouseButton, Render, ScrollDelta,
        ScrollWheelEvent, TestAppContext, TouchPhase, point, px, size,
    };

    use super::*;

    #[test]
    fn surface_rerender_snapshot_tracks_scroll_extent_changes() {
        let before = EditorSurfaceRerenderSnapshot {
            gutter_width: px(32.0),
            viewport_size: size(px(100.0), px(200.0)),
            max_scroll_offset: size(px(0.0), px(0.0)),
            reveal_armed_motion: false,
        };
        let after = EditorSurfaceRerenderSnapshot {
            gutter_width: px(32.0),
            viewport_size: size(px(100.0), px(200.0)),
            max_scroll_offset: size(px(0.0), px(400.0)),
            reveal_armed_motion: false,
        };

        assert!(before.requires_rerender_after(after));
    }

    #[test]
    fn surface_rerender_snapshot_tracks_viewport_size_changes() {
        let before = EditorSurfaceRerenderSnapshot {
            gutter_width: px(32.0),
            viewport_size: size(px(800.0), px(600.0)),
            max_scroll_offset: size(px(0.0), px(0.0)),
            reveal_armed_motion: false,
        };
        let after = EditorSurfaceRerenderSnapshot {
            gutter_width: px(32.0),
            viewport_size: size(px(100.0), px(200.0)),
            max_scroll_offset: size(px(0.0), px(0.0)),
            reveal_armed_motion: false,
        };

        assert!(before.requires_rerender_after(after));
    }

    #[test]
    fn surface_rerender_snapshot_ignores_stable_layout() {
        let snapshot = EditorSurfaceRerenderSnapshot {
            gutter_width: px(32.0),
            viewport_size: size(px(100.0), px(200.0)),
            max_scroll_offset: size(px(0.0), px(400.0)),
            reveal_armed_motion: false,
        };

        assert!(!snapshot.requires_rerender_after(snapshot));
    }

    /// A reveal that arms a tween during paint is the only way scroll motion
    /// starts after the frame driver's `scroll_needs_frames()` check has
    /// already run for this frame, so it has to schedule a follow-up frame of
    /// its own. This pins the half of that decision that lives here; the
    /// viewport half — that the tween exists and reaches its target — is
    /// pinned in `viewport.rs`.
    #[test]
    fn surface_rerender_snapshot_flags_a_paint_that_armed_scroll_motion() {
        let idle = EditorSurfaceRerenderSnapshot {
            gutter_width: px(32.0),
            viewport_size: size(px(800.0), px(600.0)),
            max_scroll_offset: size(px(0.0), px(4000.0)),
            reveal_armed_motion: false,
        };
        let armed = EditorSurfaceRerenderSnapshot {
            reveal_armed_motion: true,
            ..idle
        };

        assert!(idle.armed_scroll_motion(armed));
        // Only the transition counts. A later paint that still reports motion
        // is a frame *finishing* the tween, and scheduling another frame for
        // that would run the frame loop past its own deadline.
        assert!(!armed.armed_scroll_motion(armed));
        assert!(!armed.armed_scroll_motion(idle));
    }

    #[gpui::test]
    fn native_editor_view_draws_and_dispatches_input(cx: &mut TestAppContext) {
        let view_entity_id = cx.update(|cx| {
            let entity: Entity<Empty> = cx.new(|_| Empty);
            entity.entity_id()
        });

        let mut editor_state = EditorViewState::new(px(20.0), px(8.0));
        editor_state
            .viewport_mut()
            .set_layout(px(20.0), size(px(100.0), px(200.0)), 50);

        let painted = Rc::new(Cell::new(false));
        let overlay_seen = Rc::new(Cell::new(None));
        let saw_scroll = Rc::new(Cell::new(false));
        let saw_down = Rc::new(Cell::new(false));
        let saw_drag = Rc::new(Cell::new(false));
        let saw_up = Rc::new(Cell::new(false));
        let phases = Rc::new(RefCell::new(Vec::new()));
        let overlay_plan = CursorOverlayPlan {
            cursor_position: point(px(12.0), px(24.0)),
            cursor_size: size(px(8.0), px(20.0)),
        };

        let window = cx.add_empty_window();
        window.draw(
            point(px(0.0), px(0.0)),
            size(px(112.0), px(200.0)),
            |_, _| {
                NativeEditorView::new(
                    view_entity_id,
                    editor_state.clone(),
                    TextStyle::default(),
                    {
                        let painted = Rc::clone(&painted);
                        move |_state, _bounds, _layout, _window, _cx| {
                            painted.set(true);
                            Some(overlay_plan)
                        }
                    },
                )
                .on_cursor_overlay({
                    let overlay_seen = Rc::clone(&overlay_seen);
                    move |overlay_plan, _| overlay_seen.set(overlay_plan)
                })
                .on_scroll({
                    let saw_scroll = Rc::clone(&saw_scroll);
                    move |_, _, _| saw_scroll.set(true)
                })
                .on_pointer_selection({
                    let phases = Rc::clone(&phases);
                    move |phase, _, _| {
                        phases.borrow_mut().push(phase);
                        true
                    }
                })
                .on_mouse_down({
                    let saw_down = Rc::clone(&saw_down);
                    move |_, _| saw_down.set(true)
                })
                .on_mouse_drag({
                    let saw_drag = Rc::clone(&saw_drag);
                    move |_, _| saw_drag.set(true)
                })
                .on_mouse_up({
                    let saw_up = Rc::clone(&saw_up);
                    move |_, _| saw_up.set(true)
                })
                .into_element()
            },
        );

        window.simulate_event(ScrollWheelEvent {
            position: point(px(10.0), px(10.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-40.0))),
            modifiers: gpui::Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        window.simulate_mouse_down(
            point(px(10.0), px(10.0)),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        window.simulate_mouse_move(
            point(px(10.0), px(30.0)),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        window.simulate_mouse_up(
            point(px(10.0), px(30.0)),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );

        assert!(painted.get());
        assert_eq!(overlay_seen.get(), Some(overlay_plan));
        assert!(saw_scroll.get());
        assert!(saw_down.get());
        assert!(saw_drag.get());
        assert!(saw_up.get());
        assert_eq!(
            phases.borrow().as_slice(),
            &[
                EditorPointerSelectionPhase::Begin,
                EditorPointerSelectionPhase::Extend,
                EditorPointerSelectionPhase::End,
            ]
        );
    }

    #[gpui::test]
    fn native_editor_view_scrolls_after_initial_paint_layout(cx: &mut TestAppContext) {
        let view_entity_id = cx.update(|cx| {
            let entity: Entity<Empty> = cx.new(|_| Empty);
            entity.entity_id()
        });
        let editor_state = EditorViewState::new(px(20.0), px(8.0));

        let window = cx.add_empty_window();
        window.draw(
            point(px(0.0), px(0.0)),
            size(px(112.0), px(200.0)),
            |_, _| {
                NativeEditorView::new(
                    view_entity_id,
                    editor_state.clone(),
                    TextStyle::default(),
                    move |state, bounds, _layout, _window, _cx| {
                        state.viewport_mut().set_layout(px(20.0), bounds.size, 50);
                        None
                    },
                )
                .into_element()
            },
        );

        assert!(editor_state.viewport().max_scroll_offset().height > px(0.0));

        window.simulate_event(ScrollWheelEvent {
            position: point(px(10.0), px(10.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-40.0))),
            modifiers: gpui::Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });

        assert!(editor_state.viewport().scroll_position().y > px(0.0));
    }

    struct InitialLayoutRenderHost {
        editor_state: EditorViewState,
        render_count: Rc<Cell<usize>>,
    }

    impl Render for InitialLayoutRenderHost {
        fn render(
            &mut self,
            _window: &mut Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            self.render_count.set(self.render_count.get() + 1);
            NativeEditorView::new(
                cx.entity_id(),
                self.editor_state.clone(),
                TextStyle::default(),
                move |state, bounds, _layout, _window, _cx| {
                    state.viewport_mut().set_layout(px(20.0), bounds.size, 50);
                    None
                },
            )
        }
    }

    #[gpui::test]
    fn native_editor_view_rerenders_after_initial_paint_layout(cx: &mut TestAppContext) {
        let render_count = Rc::new(Cell::new(0));
        let render_count_clone = Rc::clone(&render_count);

        let (_host, cx) = cx.add_window_view(|_, _cx| InitialLayoutRenderHost {
            editor_state: EditorViewState::new(px(20.0), px(8.0)),
            render_count: render_count_clone,
        });

        cx.run_until_parked();

        assert!(
            render_count.get() > 1,
            "expected paint-time layout sync to request a second render"
        );
    }

    struct KeyDispatchHost {
        view_entity_id: EntityId,
        editor_state: EditorViewState,
        focus: FocusHandle,
        saw_key: Rc<Cell<bool>>,
    }

    impl Render for KeyDispatchHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            NativeEditorView::new(
                self.view_entity_id,
                self.editor_state.clone(),
                TextStyle::default(),
                |_state, _bounds, _layout, _window, _cx| None,
            )
            .track_focus(self.focus.clone())
            .on_key_down({
                let saw_key = Rc::clone(&self.saw_key);
                move |event, _, _| {
                    saw_key.set(event.keystroke.key == "a");
                    true
                }
            })
        }
    }

    #[gpui::test]
    fn native_editor_view_dispatches_key_events_from_focus(cx: &mut TestAppContext) {
        let saw_key = Rc::new(Cell::new(false));
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |_, cx| {
                let saw_key = Rc::clone(&saw_key);
                cx.new(|cx| KeyDispatchHost {
                    view_entity_id: cx.entity_id(),
                    editor_state: EditorViewState::new(px(20.0), px(8.0)),
                    focus: cx.focus_handle(),
                    saw_key,
                })
            })
            .unwrap()
        });

        window
            .update(cx, |host, window, cx| window.focus(&host.focus, cx))
            .unwrap();

        cx.dispatch_keystroke(*window, Keystroke::parse("a").unwrap());

        assert!(saw_key.get());
    }
}
