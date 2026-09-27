// ABOUTME: Native GPUI editor viewport state for pixel and visual-row scrolling
// ABOUTME: Owns GUI scroll state before it is synced into Helix view offsets

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gpui::{Bounds, Pixels, Point, Size, point, px, size};
use helix_core::{
    RopeSlice, char_idx_at_visual_offset, doc_formatter::TextFormat,
    text_annotations::TextAnnotations, visual_offset_from_block,
};
use helix_view::{Document, DocumentId, Editor, Theme, ViewId, graphics::Rect, view::ViewPosition};
use nucleotide_logging::{PerfTimer, trace};

use crate::{
    EDITOR_MINIMUM_VIEWPORT_COLUMNS, EditorDocumentMetrics, EditorDocumentMetricsCache,
    EditorDocumentMetricsCacheResolveParams, ScrollManager, scroll_animation::editor_jump_duration,
    soft_wrap_visual_position,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportScrollUpdate {
    pub changed: bool,
    pub crossed_visual_rows: isize,
    pub top_visual_row: usize,
    pub offset_within_row: Pixels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelixViewportSnapshot {
    pub anchor_line: usize,
    pub vertical_offset: usize,
    pub top_visual_row: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorViewportViewPositionPlan {
    pub top_visual_row: usize,
    pub previous_view_position: ViewPosition,
    pub view_position: ViewPosition,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorViewportViewAreaPlan {
    pub previous_area: Rect,
    pub target_area: Rect,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorViewportSurfaceUpdate {
    pub gutter_columns: u16,
    pub visual_rows: usize,
    pub soft_wrap: bool,
    pub view_area_plan: EditorViewportViewAreaPlan,
    pub view_position: ViewPosition,
    pub view_position_plan: EditorViewportViewPositionPlan,
    pub helix_view_synced: bool,
    /// This paint's cursor reveal armed motion, by any route: an instant
    /// alignment jump, a `Scrolloff` travel that stranded the cursor, or a
    /// `Scrolloff` tween that is now in flight.
    ///
    /// It is the *only* way out of paint for a tween that was armed during
    /// paint. The frame driver in `view_component.rs` consults
    /// `scroll_needs_frames()` at the top of `render`, which has already run
    /// by the time this is written, so a tween armed below it has no frame
    /// left to advance it unless the render pass is told to schedule one.
    ///
    /// This is a per-paint *level*, rewritten on every layout sync rather than
    /// latched, and `view_component.rs` compares it across the paint as a
    /// transition. A paint that merely *finishes* an armed tween also reports
    /// `true`, and must not schedule another frame for it.
    pub cursor_revealed: bool,
    pub helix_snapshot: HelixViewportSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorViewportContentUpdate {
    pub gutter_columns: u16,
    pub visual_rows: usize,
    pub soft_wrap: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EditorViewportTextFormatKey {
    soft_wrap: bool,
    tab_width: u16,
    max_wrap: u16,
    max_indent_retain: u16,
    wrap_indicator: Box<str>,
    wrap_indicator_highlight: Option<u32>,
    viewport_width: u16,
    soft_wrap_at_text_width: bool,
}

impl From<&TextFormat> for EditorViewportTextFormatKey {
    fn from(text_format: &TextFormat) -> Self {
        Self {
            soft_wrap: text_format.soft_wrap,
            tab_width: text_format.tab_width,
            max_wrap: text_format.max_wrap,
            max_indent_retain: text_format.max_indent_retain,
            wrap_indicator: text_format.wrap_indicator.clone(),
            wrap_indicator_highlight: text_format
                .wrap_indicator_highlight
                .map(|highlight| highlight.get()),
            viewport_width: text_format.viewport_width,
            soft_wrap_at_text_width: text_format.soft_wrap_at_text_width,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EditorViewportConversionBaseKey {
    document_version: i32,
    text_len: usize,
    view_id: ViewId,
    text_format: EditorViewportTextFormatKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HelixViewportSnapshotCacheKey {
    base: EditorViewportConversionBaseKey,
    view_position: ViewPosition,
}

#[derive(Clone, Debug)]
struct CachedHelixViewportSnapshot {
    key: HelixViewportSnapshotCacheKey,
    snapshot: HelixViewportSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EditorViewportViewPositionPlanCacheKey {
    base: EditorViewportConversionBaseKey,
    previous_view_position: ViewPosition,
    top_visual_row: usize,
    horizontal_offset: usize,
}

#[derive(Clone, Debug)]
struct CachedEditorViewportViewPositionPlan {
    key: EditorViewportViewPositionPlanCacheKey,
    plan: EditorViewportViewPositionPlan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocumentCursorVisualRowCacheKey {
    base: EditorViewportConversionBaseKey,
    cursor_char_idx: usize,
}

#[derive(Clone, Debug)]
struct CachedDocumentCursorVisualRow {
    key: DocumentCursorVisualRowCacheKey,
    visual_row: usize,
}

#[derive(Clone, Debug, Default)]
struct EditorViewportConversionCache {
    helix_snapshot: Option<CachedHelixViewportSnapshot>,
    view_position_plan: Option<CachedEditorViewportViewPositionPlan>,
    cursor_visual_row: Option<CachedDocumentCursorVisualRow>,
    stats: EditorViewportConversionStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditorViewportConversionStats {
    pub helix_snapshot_hits: u64,
    pub helix_snapshot_misses: u64,
    pub view_position_plan_hits: u64,
    pub view_position_plan_misses: u64,
    pub cursor_visual_row_hits: u64,
    pub cursor_visual_row_misses: u64,
    pub helix_snapshot_visual_scans: u64,
    pub view_position_char_scans: u64,
    pub cursor_visual_row_scans: u64,
}

impl EditorViewportConversionCache {
    fn helix_snapshot(
        &mut self,
        key: &HelixViewportSnapshotCacheKey,
    ) -> Option<HelixViewportSnapshot> {
        let Some(cached) = self.helix_snapshot.as_ref() else {
            self.stats.helix_snapshot_misses += 1;
            return None;
        };
        if cached.key == *key {
            self.stats.helix_snapshot_hits += 1;
            Some(cached.snapshot)
        } else {
            self.stats.helix_snapshot_misses += 1;
            None
        }
    }

    fn store_helix_snapshot(
        &mut self,
        key: HelixViewportSnapshotCacheKey,
        snapshot: HelixViewportSnapshot,
    ) {
        self.helix_snapshot = Some(CachedHelixViewportSnapshot { key, snapshot });
    }

    fn view_position_plan(
        &mut self,
        key: &EditorViewportViewPositionPlanCacheKey,
    ) -> Option<EditorViewportViewPositionPlan> {
        let Some(cached) = self.view_position_plan.as_ref() else {
            self.stats.view_position_plan_misses += 1;
            return None;
        };
        if cached.key == *key {
            self.stats.view_position_plan_hits += 1;
            Some(cached.plan)
        } else {
            self.stats.view_position_plan_misses += 1;
            None
        }
    }

    fn store_view_position_plan(
        &mut self,
        key: EditorViewportViewPositionPlanCacheKey,
        plan: EditorViewportViewPositionPlan,
    ) {
        self.view_position_plan = Some(CachedEditorViewportViewPositionPlan { key, plan });
    }

    fn cursor_visual_row(&mut self, key: &DocumentCursorVisualRowCacheKey) -> Option<usize> {
        let Some(cached) = self.cursor_visual_row.as_ref() else {
            self.stats.cursor_visual_row_misses += 1;
            return None;
        };
        if cached.key == *key {
            self.stats.cursor_visual_row_hits += 1;
            Some(cached.visual_row)
        } else {
            self.stats.cursor_visual_row_misses += 1;
            None
        }
    }

    fn store_cursor_visual_row(&mut self, key: DocumentCursorVisualRowCacheKey, visual_row: usize) {
        self.cursor_visual_row = Some(CachedDocumentCursorVisualRow { key, visual_row });
    }

    fn record_helix_snapshot_visual_scan(&mut self) {
        self.stats.helix_snapshot_visual_scans += 1;
    }

    fn record_view_position_char_scan(&mut self) {
        self.stats.view_position_char_scans += 1;
    }

    fn record_cursor_visual_row_scan(&mut self) {
        self.stats.cursor_visual_row_scans += 1;
    }

    fn stats(&self) -> EditorViewportConversionStats {
        self.stats
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorCursorReveal {
    Scrolloff,
    Center,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorViewportScrollDirection {
    Backward,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorViewportCursorTarget {
    Top,
    Center,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorViewportCursorRequest {
    pub target: EditorViewportCursorTarget,
    pub count: usize,
}

impl EditorViewportCursorRequest {
    pub fn target_visual_row(
        self,
        top_visual_row: usize,
        visible_rows: usize,
        content_visual_rows: usize,
        scrolloff: usize,
    ) -> usize {
        let visible_rows = visible_rows.max(1);
        let content_visual_rows = content_visual_rows.max(1);
        let last_content_row = content_visual_rows.saturating_sub(1);
        let last_visible_row = top_visual_row
            .saturating_add(visible_rows.saturating_sub(1))
            .min(last_content_row);
        let last_visible_offset = last_visible_row.saturating_sub(top_visual_row);
        let scrolloff = scrolloff.min(visible_rows.saturating_sub(1) / 2);
        let count_offset = self.count.max(1).saturating_sub(1);

        let target = match self.target {
            EditorViewportCursorTarget::Top => top_visual_row
                .saturating_add(scrolloff)
                .saturating_add(count_offset),
            EditorViewportCursorTarget::Center => {
                top_visual_row.saturating_add(last_visible_offset / 2)
            }
            EditorViewportCursorTarget::Bottom => top_visual_row.saturating_add(
                last_visible_offset.saturating_sub(scrolloff.saturating_add(count_offset)),
            ),
        };

        target
            .max(top_visual_row.saturating_add(scrolloff))
            .min(top_visual_row.saturating_add(last_visible_offset.saturating_sub(scrolloff)))
            .min(last_content_row)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorViewportScrollRequest {
    VisualRows(isize),
    VisualPages(isize),
    VisualPageFraction { pages: isize, divisor: usize },
    VisualPageWithCursor { pages: isize, divisor: usize },
    CursorReveal(EditorCursorReveal),
}

impl EditorViewportScrollRequest {
    pub fn page_cursor_sync_direction(self) -> Option<EditorViewportScrollDirection> {
        match self {
            Self::VisualPages(pages) if pages > 0 => Some(EditorViewportScrollDirection::Forward),
            Self::VisualPages(pages) if pages < 0 => Some(EditorViewportScrollDirection::Backward),
            Self::VisualPageFraction { pages, .. } if pages > 0 => {
                Some(EditorViewportScrollDirection::Forward)
            }
            Self::VisualPageFraction { pages, .. } if pages < 0 => {
                Some(EditorViewportScrollDirection::Backward)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EditorViewportContentLayout<'a> {
    pub theme: Option<&'a Theme>,
    pub bounds: Bounds<Pixels>,
    pub cell_width: Pixels,
    pub minimum_columns: u16,
    pub extra_gutter_columns: u16,
}

impl<'a> EditorViewportContentLayout<'a> {
    pub fn for_editor(
        theme: Option<&'a Theme>,
        bounds: Bounds<Pixels>,
        cell_width: Pixels,
    ) -> Self {
        Self {
            theme,
            bounds,
            cell_width,
            minimum_columns: EDITOR_MINIMUM_VIEWPORT_COLUMNS,
            extra_gutter_columns: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EditorViewportSurfaceLayout<'a> {
    pub theme: Option<&'a Theme>,
    pub bounds: Bounds<Pixels>,
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub minimum_columns: u16,
    pub extra_gutter_columns: u16,
    pub scrolloff: usize,
    pub cursor_reveal: Option<EditorCursorReveal>,
}

impl<'a> EditorViewportSurfaceLayout<'a> {
    pub fn for_editor(
        theme: Option<&'a Theme>,
        bounds: Bounds<Pixels>,
        cell_width: Pixels,
        line_height: Pixels,
        scrolloff: usize,
        cursor_reveal: Option<EditorCursorReveal>,
    ) -> Self {
        Self {
            theme,
            bounds,
            cell_width,
            line_height,
            minimum_columns: EDITOR_MINIMUM_VIEWPORT_COLUMNS,
            extra_gutter_columns: 0,
            scrolloff,
            cursor_reveal,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EditorViewport {
    scroll: ScrollManager,
    cursor_reveal_request: Rc<Cell<Option<EditorCursorReveal>>>,
    cell_width: Rc<Cell<Pixels>>,
    document_metrics_cache: Rc<RefCell<EditorDocumentMetricsCache>>,
    conversion_cache: Rc<RefCell<EditorViewportConversionCache>>,
}

impl EditorViewport {
    pub fn new(line_height: Pixels) -> Self {
        Self {
            scroll: ScrollManager::new(line_height),
            cursor_reveal_request: Rc::new(Cell::new(None)),
            cell_width: Rc::new(Cell::new(px(1.0))),
            document_metrics_cache: Rc::new(RefCell::new(EditorDocumentMetricsCache::default())),
            conversion_cache: Rc::new(RefCell::new(EditorViewportConversionCache::default())),
        }
    }

    pub fn invalidate_document_lines(
        &self,
        document_id: DocumentId,
        old_lines: std::ops::Range<usize>,
        new_lines: std::ops::Range<usize>,
    ) {
        self.document_metrics_cache
            .borrow_mut()
            .invalidate_document_lines(document_id, old_lines, new_lines);
    }

    pub fn invalidate_document_annotations(&self, document_id: DocumentId) {
        self.document_metrics_cache
            .borrow_mut()
            .invalidate_document_annotations(document_id);
    }

    pub fn set_layout(
        &mut self,
        line_height: Pixels,
        viewport_size: Size<Pixels>,
        content_visual_rows: usize,
    ) {
        self.set_line_height(line_height);
        self.set_viewport_size(viewport_size);
        self.set_content_visual_rows(content_visual_rows);
    }

    pub fn set_line_height(&mut self, line_height: Pixels) {
        self.scroll.set_line_height(line_height);
    }

    pub fn line_height(&self) -> Pixels {
        self.scroll.line_height()
    }

    pub fn set_viewport_size(&mut self, size: Size<Pixels>) {
        self.scroll.set_viewport_size(size);
    }

    pub fn set_cell_width(&mut self, cell_width: Pixels) {
        self.cell_width.set(cell_width.max(px(1.0)));
    }

    pub fn set_content_visual_rows(&mut self, rows: usize) {
        self.scroll.set_total_lines(rows.max(1));
    }

    pub fn set_content_width(&mut self, width: Pixels) {
        self.scroll.set_content_width(width);
    }

    pub fn content_visual_rows(&self) -> usize {
        self.scroll.total_lines()
    }

    pub fn conversion_stats(&self) -> EditorViewportConversionStats {
        self.conversion_cache.borrow().stats()
    }

    pub fn max_scroll_offset(&self) -> Size<Pixels> {
        self.scroll.max_scroll_offset()
    }

    pub fn scroll_position(&self) -> Point<Pixels> {
        self.scroll.scroll_position()
    }

    pub fn scroll_offset(&self) -> Point<Pixels> {
        self.scroll.scroll_offset()
    }

    pub fn set_scroll_offset_from_scrollbar(&self, offset: Point<Pixels>) {
        self.scroll.set_scroll_offset(offset, "scrollbar_offset");
    }

    pub fn viewport_bounds(&self) -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), self.scroll.viewport_size())
    }

    pub fn has_pending_view_sync(&self) -> bool {
        self.scroll.has_pending_view_sync()
    }

    pub fn clear_pending_view_sync(&self) {
        self.scroll.clear_pending_view_sync();
    }

    pub fn scroll_by_delta(&self, delta: Point<Pixels>) -> ViewportScrollUpdate {
        self.scroll_by_delta_at(delta, std::time::Instant::now())
    }

    /// [`Self::scroll_by_delta`] with an injected tween clock.
    ///
    /// The in-gesture wheel tween is armed with `now`, so its deadline is exactly
    /// `now + TWEEN_MS` and sampling at `now + k` measures `k` of easing. That is
    /// what makes the whole wheel sequence reproducible from an injected
    /// `Instant`, including a notch that arrives while a tween is in flight.
    pub(crate) fn scroll_by_delta_at(
        &self,
        delta: Point<Pixels>,
        now: std::time::Instant,
    ) -> ViewportScrollUpdate {
        let (changed, crossed_visual_rows) = self.scroll.scroll_by_delta_at(delta, now);

        // Report the destination, not the live position, for the same reason
        // `scroll_by_visual_rows` does. The in-gesture tween has not moved the
        // viewport yet, but the caller still has to learn that the notch moved the
        // top visual row: it decides `cx.notify()` and feeds the Helix cursor sync.
        // A `changed` of false here is exactly what makes the wheel handler's
        // early return swallow the whole update — no repaint, so no frame, so the
        // tween that was just armed never runs and the view never scrolls.
        let new_position = self.scroll.destination_position();
        let new_top_visual_row = self.scroll.pixels_to_anchor(new_position.y);

        ViewportScrollUpdate {
            changed,
            crossed_visual_rows,
            top_visual_row: new_top_visual_row,
            offset_within_row: self.offset_within_row_for(new_position.y),
        }
    }

    pub fn apply_scroll_request(
        &self,
        request: EditorViewportScrollRequest,
    ) -> ViewportScrollUpdate {
        match request {
            EditorViewportScrollRequest::VisualRows(rows) => self.scroll_by_visual_rows(rows),
            EditorViewportScrollRequest::VisualPages(pages) => self.scroll_by_visual_pages(pages),
            EditorViewportScrollRequest::VisualPageFraction { pages, divisor }
            | EditorViewportScrollRequest::VisualPageWithCursor { pages, divisor } => {
                self.scroll_by_visual_page_fraction(pages, divisor)
            }
            EditorViewportScrollRequest::CursorReveal(reveal) => {
                // Cursor correctness beats smoothness: a reveal jump has to land
                // on the cursor immediately, so any in-flight scroll tween is
                // dropped before the reveal is queued.
                self.scroll.animation_cancel("cursor_reveal_request");
                self.request_cursor_reveal(reveal);
                ViewportScrollUpdate {
                    changed: false,
                    crossed_visual_rows: 0,
                    top_visual_row: self.top_visual_row(),
                    offset_within_row: self.offset_within_row(),
                }
            }
        }
    }

    pub fn scroll_by_visual_pages(&self, pages: isize) -> ViewportScrollUpdate {
        self.scroll_by_visual_page_fraction(pages, 1)
    }

    pub fn scroll_by_visual_page_fraction(
        &self,
        pages: isize,
        divisor: usize,
    ) -> ViewportScrollUpdate {
        let visible_rows = self.visible_visual_rows() / divisor.max(1);
        let visible_rows = isize::try_from(visible_rows).unwrap_or(isize::MAX);
        self.scroll_by_visual_rows(visible_rows.saturating_mul(pages))
    }

    pub fn scroll_by_visual_rows(&self, rows: isize) -> ViewportScrollUpdate {
        let old_position = self.scroll_position();
        let old_top_visual_row = self.top_visual_row();

        if rows != 0 {
            // A discrete jump is a deliberate, absolute move. Any cursor-follow
            // inertia still collecting from an earlier `j`/`k` run must be dropped
            // here: its carry is aimed at the margin that was current *before* this
            // jump, so letting the gesture window close afterwards would ease the
            // view toward a stale position the user never asked for. The carry's
            // landing check only guards a carry that is already in flight, not a
            // gesture that is still collecting, so the clear belongs here.
            self.scroll.clear_scrolloff_inertia();
            let delta_y = self.scroll.line_height() * rows as f32;
            let target = point(old_position.x, old_position.y + delta_y);
            if self.scroll.scroll_animation_enabled() {
                self.scroll
                    .animate_scroll_to(target, editor_jump_duration(rows));
            } else {
                self.scroll.set_scroll_position(target, "instant_scroll_fallback");
            }
        }

        // Report the destination, not the live position. A smooth tween has not
        // moved the viewport yet, but the caller still has to learn that the
        // request moved the top visual row: it decides `cx.notify()` and feeds
        // the Helix cursor sync, so reporting the current row would make
        // `page_down` silently stop moving the cursor with no repaint at all.
        let new_position = self.scroll.destination_position();
        let new_top_visual_row = self.scroll.pixels_to_anchor(new_position.y);

        ViewportScrollUpdate {
            changed: old_position != new_position,
            crossed_visual_rows: new_top_visual_row as isize - old_top_visual_row as isize,
            top_visual_row: new_top_visual_row,
            offset_within_row: self.offset_within_row_for(new_position.y),
        }
    }

    pub fn scroll_to_vertical_position_from_scrollbar(&self, y: Pixels) -> ViewportScrollUpdate {
        let old_position = self.scroll_position();
        let old_top_visual_row = self.top_visual_row();

        self.scroll
            .set_scroll_position(point(old_position.x, y), "vertical_scrollbar_drag");

        let new_position = self.scroll_position();
        let new_top_visual_row = self.top_visual_row();

        ViewportScrollUpdate {
            changed: old_position != new_position,
            crossed_visual_rows: new_top_visual_row as isize - old_top_visual_row as isize,
            top_visual_row: new_top_visual_row,
            offset_within_row: self.offset_within_row(),
        }
    }

    pub fn scroll_to_horizontal_position_from_scrollbar(&self, x: Pixels) -> ViewportScrollUpdate {
        let old_position = self.scroll_position();
        let old_top_visual_row = self.top_visual_row();

        self.scroll
            .set_scroll_position(point(x, old_position.y), "horizontal_scrollbar_drag");

        let new_position = self.scroll_position();
        let new_top_visual_row = self.top_visual_row();

        ViewportScrollUpdate {
            changed: old_position != new_position,
            crossed_visual_rows: new_top_visual_row as isize - old_top_visual_row as isize,
            top_visual_row: new_top_visual_row,
            offset_within_row: self.offset_within_row(),
        }
    }

    pub fn request_cursor_reveal(&self, reveal: EditorCursorReveal) {
        self.cursor_reveal_request.set(Some(reveal));
    }

    pub fn pending_cursor_reveal_request(&self) -> Option<EditorCursorReveal> {
        self.cursor_reveal_request.get()
    }

    pub fn take_cursor_reveal_request(&self) -> Option<EditorCursorReveal> {
        self.cursor_reveal_request.replace(None)
    }

    pub fn ensure_visual_row_visible(
        &self,
        visual_row: usize,
        scrolloff: usize,
    ) -> ViewportScrollUpdate {
        self.reveal_visual_row(visual_row, EditorCursorReveal::Scrolloff, scrolloff)
    }

    /// Bring `visual_row` into view under `reveal`'s alignment rule.
    ///
    /// `Scrolloff` is the only reveal that eases. It is also the only one that
    /// fires constantly — every `j`/`k` that walks the cursor off the scrolloff
    /// margin — and the only one whose travel is bounded by that margin, so its
    /// tween is a one-row move. `Top`/`Center`/`Bottom` are deliberate
    /// alignment jumps and stay instant, as does the whole
    /// `apply_scroll_request` route, which only queues a request.
    ///
    /// # The band origin is the destination, not the live row
    ///
    /// The `Scrolloff` band is measured from
    /// [`ScrollManager::destination_position`], which resolves to the live
    /// position while idle and to the tween's target while one is in flight.
    /// `top_visual_row()` would be a mid-tween row, and a band built around a
    /// row the viewport is still passing through judges a cursor that is
    /// comfortably inside the *destination* band to be outside it. The traced
    /// `page_down` case put the destination at row 40 with the cursor at row 45
    /// and dragged the view to row 11 instead.
    ///
    /// # Applying the target retargets, never cancels
    ///
    /// A target is applied through [`ScrollManager::animate_scroll_to`], which
    /// arms a fresh tween from the live position and replaces whatever was in
    /// flight. The instant `set_scroll_position` path arrives with
    /// `from_native_view = true` and cancels unconditionally, so it cannot be
    /// used here: it would take down the very tween the reveal is being
    /// applied on top of.
    pub fn reveal_visual_row(
        &self,
        visual_row: usize,
        reveal: EditorCursorReveal,
        scrolloff: usize,
    ) -> ViewportScrollUpdate {
        let old_position = self.scroll_position();
        let old_top_visual_row = self.top_visual_row();
        let visible_rows = self.visible_visual_rows();
        let tween_active = self.scroll_animation_active();
        // Band origin. `destination_position()` is the live position while idle
        // and the tween's target while one is in flight, so this is the row the
        // view *will* be at when the reveal lands — the only row a scrolloff
        // band can be meaningfully measured against.
        let tween_destination_row =
            self.scroll.pixels_to_anchor(self.scroll.destination_position().y);
        // The scrolloff margin, clamped so a viewport too short to hold two
        // margins plus a row still has a usable band. Only `Scrolloff` reads it;
        // the other three rules align to an absolute row.
        let margin = scrolloff.min(visible_rows.saturating_sub(1) / 2);

        // Entry log. `old_top_visual_row` is the *live* row, so during a tween
        // it is mid-flight rather than the destination; comparing it against
        // `tween_destination_row` is what shows whether the scrolloff band below
        // is being evaluated against a mid-tween origin.
        trace!(
            reason = "reveal_visual_row",
            old_top_visual_row,
            tween_active,
            tween_destination_row,
            visual_row,
            visible_rows,
            scrolloff,
            reveal = ?reveal,
            "EditorViewport reveal entry"
        );

        let target_top = match reveal {
            EditorCursorReveal::Scrolloff => {
                let lower_bound = tween_destination_row.saturating_add(margin);
                let upper_bound =
                    tween_destination_row.saturating_add(visible_rows.saturating_sub(margin));

                if visual_row < lower_bound {
                    Some(visual_row.saturating_sub(margin))
                } else if visual_row >= upper_bound {
                    Some(
                        visual_row
                            .saturating_add(margin)
                            .saturating_add(1)
                            .saturating_sub(visible_rows),
                    )
                } else {
                    None
                }
            }
            EditorCursorReveal::Center => Some(visual_row.saturating_sub(visible_rows / 2)),
            EditorCursorReveal::Top => Some(visual_row),
            EditorCursorReveal::Bottom => {
                Some(visual_row.saturating_sub(visible_rows.saturating_sub(1)))
            }
        };

        // Rows the view has to travel to reach the target the band chose,
        // measured from the row the band itself was measured against. For the
        // ordinary `Scrolloff` case this is exactly 1: the band fires as the
        // cursor crosses the margin, and the target restores that margin.
        let travel_rows = target_top
            .map(|target| target as isize - tween_destination_row as isize)
            .unwrap_or(0);

        // Ease only when the cursor survives the flight inside the painted
        // band. The hazard is the cursor leaving that band, not the distance
        // travelled: a line outside the rendered range is not painted at all
        // (`document_frame_painter.rs` returns `None` for it), so the cursor
        // would simply vanish for the duration of the tween.
        //
        // The band spans `visible_rows` rows, the cursor is at most `margin`
        // rows inside whichever edge fired, and the view is about to travel
        // `travel_rows`, so the cursor ends up off the far edge exactly when
        // `travel_rows > visible_rows - margin`. That threshold self-scales
        // with the window instead of being a fixed row count: a one-row
        // `Scrolloff` reveal is always far inside the band and must never snap,
        // while a `G` of thousands of rows must.
        //
        // `visible_rows >= 1` and `margin <= (visible_rows - 1) / 2`, so the
        // subtraction cannot underflow.
        let eased = target_top.is_some()
            && matches!(reveal, EditorCursorReveal::Scrolloff)
            && travel_rows.unsigned_abs() <= visible_rows - margin
            && self.smooth_scrolling_enabled();

        if let Some(target_top) = target_top {
            let target = point(old_position.x, self.scroll.anchor_to_pixels(target_top));

            if eased {
                // `animate_scroll_to` arms a *fresh*, full-duration tween from
                // the live position, replacing whatever was in flight. That is
                // deliberate: `retarget_from` carries the remaining budget
                // forward instead, and holding `j` re-arms roughly every 33ms,
                // which would collapse `remaining` onto its one-frame floor and
                // pin the tween at its origin so the view never travels. Same
                // argument as the wheel notch in `scroll_manager.rs`.
                //
                // The duration is the one every other viewport tween uses. The
                // distance already bounds the cost — one row is 66ms — so a
                // second constant ladder for the same value would be a second
                // thing to tune wrong.
                //
                // `ease_scrolloff_reveal_to` is `animate_scroll_to` plus the
                // cursor-follow gesture bookkeeping, in one call so that this
                // paint path stays a single statement: an eased reveal is
                // exactly what a `j`/`k` hold looks like from here, and it is the
                // only thing that may contribute to the gesture.
                self.scroll.ease_scrolloff_reveal_to(
                    target,
                    travel_rows,
                    target_top,
                    editor_jump_duration(travel_rows),
                );
            } else {
                // Still the instant path, and still the one that cancels: this
                // is where a deliberate alignment jump, a `Scrolloff` travel
                // that would strand the cursor, and the no-smooth-scrolling
                // configuration all land.
                //
                // A snap is a jump rather than a glide, so it resets the
                // cursor-follow gesture instead of joining it — and the instant
                // write below cancels whatever tween was in flight, which kills
                // a carry of that gesture in the same stroke.
                self.scroll.clear_scrolloff_inertia();
                self.scroll
                    .set_scroll_position(target, "reveal_visual_row");
            }
        }

        // Result log. `target_top` is the row the band chose; `eased` says
        // whether it was reached by a tween or by an instant write. `None`
        // means the band judged the cursor already visible and nothing moved.
        trace!(
            reason = "reveal_visual_row",
            old_top_visual_row,
            tween_active,
            tween_destination_row,
            visual_row,
            target_top = ?target_top,
            travel_rows,
            eased,
            new_top_visual_row = self.top_visual_row(),
            "EditorViewport reveal applied"
        );

        // Report the destination, not the live position, for the same reason
        // `scroll_by_visual_rows` does: an armed tween has not moved the
        // viewport yet, but the caller still has to learn that the request
        // moved the top visual row, because it decides `cx.notify()` and feeds
        // the Helix cursor sync. Reporting the live row would make a one-row
        // eased reveal report `changed: false`, and the frame that drives the
        // tween would never be requested.
        let new_position = self.scroll.destination_position();
        let new_top_visual_row = self.scroll.pixels_to_anchor(new_position.y);

        ViewportScrollUpdate {
            changed: old_position != new_position,
            crossed_visual_rows: new_top_visual_row as isize - old_top_visual_row as isize,
            top_visual_row: new_top_visual_row,
            offset_within_row: self.offset_within_row_for(new_position.y),
        }
    }

    pub fn visible_visual_rows(&self) -> usize {
        let line_height = self.line_height();
        if f32::from(line_height) <= 0.0 {
            return 1;
        }

        ((self.scroll.viewport_size().height / line_height).floor() as usize).max(1)
    }

    pub fn sync_from_helix_top_visual_row(&self, top_visual_row: usize) {
        let current = self.scroll_position();
        let y = self.scroll.anchor_to_pixels(top_visual_row);
        self.scroll
            .set_scroll_position_from_view_sync_preserving_subrow_offset(point(current.x, y));
    }

    pub fn sync_from_helix_view(
        &self,
        document: &Document,
        view: &helix_view::View,
        view_id: ViewId,
        text_format: &TextFormat,
    ) -> HelixViewportSnapshot {
        let view_position = document.view_offset(view_id);
        let key = HelixViewportSnapshotCacheKey {
            base: viewport_conversion_base_key(document, view_id, text_format),
            view_position,
        };

        if let Some(snapshot) = self.conversion_cache.borrow_mut().helix_snapshot(&key) {
            self.sync_from_helix_horizontal_offset(view_position.horizontal_offset, text_format);
            self.sync_from_helix_top_visual_row(snapshot.top_visual_row);
            return snapshot;
        }

        self.conversion_cache
            .borrow_mut()
            .record_helix_snapshot_visual_scan();
        let annotations = view.text_annotations(document, None);
        let snapshot = helix_viewport_snapshot(
            document.text().slice(..),
            view_position,
            text_format,
            &annotations,
        );
        self.conversion_cache
            .borrow_mut()
            .store_helix_snapshot(key, snapshot);
        self.sync_from_helix_horizontal_offset(view_position.horizontal_offset, text_format);
        self.sync_from_helix_top_visual_row(snapshot.top_visual_row);
        snapshot
    }

    fn sync_from_helix_horizontal_offset(
        &self,
        horizontal_offset: usize,
        text_format: &TextFormat,
    ) {
        let x = if text_format.soft_wrap {
            px(0.0)
        } else {
            self.cell_width.get() * horizontal_offset as f32
        };
        // Axis-only write. This sync cannot move the viewport vertically, so it
        // must not cancel a vertical scroll tween; it used to route through the
        // general view-sync setter, whose then-unconditional `animation.cancel`
        // killed an in-flight tween on every painted frame.
        self.scroll.set_horizontal_scroll_offset_from_view_sync(x);
    }

    fn horizontal_offset_columns(&self, text_format: &TextFormat) -> usize {
        if text_format.soft_wrap {
            return 0;
        }

        let cell_width = self.cell_width.get();
        if cell_width <= px(0.0) {
            return 0;
        }

        (self.scroll_position().x / cell_width).floor() as usize
    }

    pub fn sync_view_position(
        &self,
        document: &mut Document,
        view: &helix_view::View,
        view_id: ViewId,
        text_format: &TextFormat,
    ) -> bool {
        self.sync_view_position_with_plan(document, view, view_id, text_format, true)
            .0
    }

    fn sync_view_position_with_plan(
        &self,
        document: &mut Document,
        view: &helix_view::View,
        view_id: ViewId,
        text_format: &TextFormat,
        use_native_horizontal_offset: bool,
    ) -> (bool, EditorViewportViewPositionPlan) {
        let plan = if use_native_horizontal_offset {
            self.plan_view_position_with_horizontal_offset(
                document,
                view,
                view_id,
                text_format,
                self.horizontal_offset_columns(text_format),
            )
        } else {
            self.plan_view_position(document, view, view_id, text_format)
        };
        if !plan.changed {
            return (false, plan);
        }

        trace!(
            view_id = ?view_id,
            top_visual_row = plan.top_visual_row,
            old_anchor = plan.previous_view_position.anchor,
            new_anchor = plan.view_position.anchor,
            old_vertical_offset = plan.previous_view_position.vertical_offset,
            new_vertical_offset = plan.view_position.vertical_offset,
            "Syncing GUI scroll position to Helix view"
        );

        let synced = apply_view_position_plan(document, view_id, plan);
        if synced {
            let synced_plan = EditorViewportViewPositionPlan {
                top_visual_row: plan.top_visual_row,
                previous_view_position: plan.view_position,
                view_position: plan.view_position,
                changed: false,
            };
            self.cache_view_position_plan(document, view_id, text_format, synced_plan);
        }

        (synced, plan)
    }

    pub fn sync_content_layout(
        &mut self,
        document: &Document,
        view: &helix_view::View,
        layout: EditorViewportContentLayout<'_>,
    ) -> EditorViewportContentUpdate {
        let (gutter_columns, metrics) = {
            let mut metrics_cache = self.document_metrics_cache.borrow_mut();
            editor_viewport_content_metrics(&mut metrics_cache, document, view, layout)
        };
        self.set_cell_width(layout.cell_width);
        self.set_content_visual_rows(metrics.visual_rows);
        self.set_viewport_size(editor_text_viewport_size_for_bounds(
            layout.bounds,
            gutter_columns,
            layout.cell_width,
        ));
        self.set_content_width(metrics.content_columns as f32 * layout.cell_width);

        EditorViewportContentUpdate {
            gutter_columns,
            visual_rows: metrics.visual_rows,
            soft_wrap: metrics.soft_wrap,
        }
    }

    pub fn sync_surface_layout(
        &mut self,
        editor: &mut Editor,
        doc_id: DocumentId,
        view_id: ViewId,
        layout: EditorViewportSurfaceLayout<'_>,
    ) -> Option<EditorViewportSurfaceUpdate> {
        let mut view = editor.tree.try_get(view_id)?.clone();
        let update = {
            let document = editor.document_mut(doc_id)?;
            self.sync_surface_layout_for_view(document, &mut view, view_id, layout)
        };
        // Keep the resized view local to this surface. The Helix tree owns split
        // geometry; writing per-pane paint bounds back into it causes layout
        // feedback between independently rendered panes.

        Some(update)
    }

    pub fn sync_surface_layout_for_view(
        &mut self,
        document: &mut Document,
        view: &mut helix_view::View,
        view_id: ViewId,
        layout: EditorViewportSurfaceLayout<'_>,
    ) -> EditorViewportSurfaceUpdate {
        let _timer = PerfTimer::new("EditorViewport::sync_surface_layout_for_view")
            .with_warn_threshold(Duration::from_millis(8));
        self.set_line_height(layout.line_height);
        self.set_cell_width(layout.cell_width);
        self.set_viewport_size(editor_viewport_size_for_bounds(layout.bounds));

        let content_layout = EditorViewportContentLayout {
            theme: layout.theme,
            bounds: layout.bounds,
            cell_width: layout.cell_width,
            minimum_columns: layout.minimum_columns,
            extra_gutter_columns: layout.extra_gutter_columns,
        };
        let (gutter_columns, metrics) = {
            let mut metrics_cache = self.document_metrics_cache.borrow_mut();
            editor_viewport_surface_metrics(
                &mut metrics_cache,
                document,
                view,
                content_layout,
                self.visible_visual_rows(),
            )
        };
        self.set_content_visual_rows(metrics.visual_rows);
        let viewport_size =
            editor_text_viewport_size_for_bounds(layout.bounds, gutter_columns, layout.cell_width);
        self.set_viewport_size(viewport_size);
        self.set_content_width(metrics.content_columns as f32 * layout.cell_width);
        let view_area_plan = helix_view_area_plan_for_surface(
            view.area,
            gutter_columns,
            metrics.viewport_columns,
            self.visible_visual_rows(),
        );
        apply_helix_view_area_plan(view, view_area_plan);

        let mut synced_view_position_plan = None;
        let mut helix_view_synced = if self.has_pending_view_sync() {
            let (synced, plan) = self.sync_view_position_with_plan(
                document,
                view,
                view_id,
                &metrics.text_format,
                true,
            );
            self.clear_pending_view_sync();
            if synced {
                synced_view_position_plan = Some(plan);
            }
            synced
        } else {
            false
        };

        let cursor_revealed = if let Some(cursor_reveal) = layout.cursor_reveal {
            let cursor_visual_row =
                self.document_cursor_visual_row(document, view, view_id, &metrics.text_format);
            let scroll_update =
                self.reveal_visual_row(cursor_visual_row, cursor_reveal, layout.scrolloff);

            let (synced, plan) = self.sync_view_position_with_plan(
                document,
                view,
                view_id,
                &metrics.text_format,
                false,
            );
            helix_view_synced |= synced;
            if synced {
                synced_view_position_plan = Some(plan);
            }

            scroll_update.changed
        } else {
            false
        };

        let (helix_snapshot, view_position_plan) = if let Some(plan) = synced_view_position_plan
            .filter(|plan| plan.view_position == document.view_offset(view_id))
        {
            let snapshot = helix_viewport_snapshot_for_synced_plan(document, plan);
            let view_position_plan = EditorViewportViewPositionPlan {
                top_visual_row: plan.top_visual_row,
                previous_view_position: plan.view_position,
                view_position: plan.view_position,
                changed: false,
            };
            self.cache_helix_viewport_snapshot(
                document,
                view_id,
                &metrics.text_format,
                plan.view_position,
                snapshot,
            );
            self.cache_view_position_plan(
                document,
                view_id,
                &metrics.text_format,
                view_position_plan,
            );
            (snapshot, view_position_plan)
        } else {
            let helix_snapshot =
                self.sync_from_helix_view(document, view, view_id, &metrics.text_format);
            let view_position_plan =
                self.plan_view_position(document, view, view_id, &metrics.text_format);
            (helix_snapshot, view_position_plan)
        };
        let view_position = view_position_plan.view_position;

        EditorViewportSurfaceUpdate {
            gutter_columns,
            visual_rows: metrics.visual_rows,
            soft_wrap: metrics.soft_wrap,
            view_area_plan,
            view_position,
            view_position_plan,
            helix_view_synced,
            cursor_revealed,
            helix_snapshot,
        }
    }

    pub fn plan_view_position(
        &self,
        document: &Document,
        view: &helix_view::View,
        view_id: ViewId,
        text_format: &TextFormat,
    ) -> EditorViewportViewPositionPlan {
        let horizontal_offset = document.view_offset(view_id).horizontal_offset;
        self.plan_view_position_with_horizontal_offset(
            document,
            view,
            view_id,
            text_format,
            horizontal_offset,
        )
    }

    fn plan_view_position_with_horizontal_offset(
        &self,
        document: &Document,
        view: &helix_view::View,
        view_id: ViewId,
        text_format: &TextFormat,
        horizontal_offset: usize,
    ) -> EditorViewportViewPositionPlan {
        let previous_view_position = document.view_offset(view_id);
        let top_visual_row = self.top_visual_row();
        let key = EditorViewportViewPositionPlanCacheKey {
            base: viewport_conversion_base_key(document, view_id, text_format),
            previous_view_position,
            top_visual_row,
            horizontal_offset,
        };

        if let Some(plan) = self.conversion_cache.borrow_mut().view_position_plan(&key) {
            return plan;
        }

        self.conversion_cache
            .borrow_mut()
            .record_view_position_char_scan();
        let annotations = view.text_annotations(document, None);
        let plan = view_position_plan_for_top_visual_row(
            document.text().slice(..),
            previous_view_position,
            top_visual_row,
            horizontal_offset,
            text_format,
            &annotations,
        );
        self.conversion_cache
            .borrow_mut()
            .store_view_position_plan(key, plan);
        plan
    }

    fn cache_view_position_plan(
        &self,
        document: &Document,
        view_id: ViewId,
        text_format: &TextFormat,
        plan: EditorViewportViewPositionPlan,
    ) {
        let key = EditorViewportViewPositionPlanCacheKey {
            base: viewport_conversion_base_key(document, view_id, text_format),
            previous_view_position: plan.previous_view_position,
            top_visual_row: plan.top_visual_row,
            horizontal_offset: plan.view_position.horizontal_offset,
        };
        self.conversion_cache
            .borrow_mut()
            .store_view_position_plan(key, plan);
    }

    fn cache_helix_viewport_snapshot(
        &self,
        document: &Document,
        view_id: ViewId,
        text_format: &TextFormat,
        view_position: ViewPosition,
        snapshot: HelixViewportSnapshot,
    ) {
        let key = HelixViewportSnapshotCacheKey {
            base: viewport_conversion_base_key(document, view_id, text_format),
            view_position,
        };
        self.conversion_cache
            .borrow_mut()
            .store_helix_snapshot(key, snapshot);
    }

    fn document_cursor_visual_row(
        &self,
        document: &Document,
        view: &helix_view::View,
        view_id: ViewId,
        text_format: &TextFormat,
    ) -> usize {
        let text = document.text().slice(..);
        let cursor_char_idx = document.selection(view_id).primary().cursor(text);
        let key = DocumentCursorVisualRowCacheKey {
            base: viewport_conversion_base_key(document, view_id, text_format),
            cursor_char_idx,
        };

        if let Some(visual_row) = self.conversion_cache.borrow_mut().cursor_visual_row(&key) {
            return visual_row;
        }

        self.conversion_cache
            .borrow_mut()
            .record_cursor_visual_row_scan();
        let visual_row =
            document_cursor_visual_row_for_cursor(document, view, text_format, cursor_char_idx);
        self.conversion_cache
            .borrow_mut()
            .store_cursor_visual_row(key, visual_row);
        visual_row
    }

    pub fn top_visual_row(&self) -> usize {
        self.scroll.pixels_to_anchor(self.scroll_position().y)
    }

    pub fn offset_within_row(&self) -> Pixels {
        self.scroll.vertical_offset_within_line()
    }

    fn offset_within_row_for(&self, position_y: Pixels) -> Pixels {
        self.scroll.vertical_offset_within_line_at(position_y)
    }

    /// Enable or disable smooth scrolling of discrete scroll requests.
    ///
    /// Disabled by default, in which case every request lands instantly.
    pub fn set_smooth_scrolling(&self, enabled: bool) {
        self.scroll.set_scroll_animation_enabled(enabled);
    }

    pub fn smooth_scrolling_enabled(&self) -> bool {
        self.scroll.scroll_animation_enabled()
    }

    /// Whether a scroll tween is currently in flight. Cheap enough to use as a
    /// per-frame guard before driving the animation.
    pub fn scroll_animation_active(&self) -> bool {
        self.scroll.scroll_animation_active()
    }

    /// Whether the render pass must keep requesting frames to make scroll
    /// progress.
    ///
    /// This is about *desired* motion, not per-frame movement: a frame that
    /// moves no pixels still has to schedule the next one, otherwise a tween
    /// stalls. Do not collapse this into `scroll_animation_active()`; it also
    /// covers a wheel gesture that is waiting out its idle window, and a
    /// cursor-follow gesture that is doing the same or is mid-flight.
    pub fn scroll_needs_frames(&self) -> bool {
        self.scroll.scroll_needs_frames()
    }

    /// Advance an in-flight tween to the current instant, returning whether the
    /// scroll position changed. Call once per rendered frame.
    pub fn advance_scroll_animation(&self) -> bool {
        self.scroll.advance_scroll_animation()
    }

    /// Deterministic variant of [`Self::advance_scroll_animation`] for tests.
    #[cfg(test)]
    pub(crate) fn advance_scroll_animation_at(&self, now: std::time::Instant) -> bool {
        self.scroll.advance_scroll_animation_at(now)
    }

    /// Enable or disable the eased glide that follows a wheel gesture.
    ///
    /// The 1:1 wheel path is unaffected either way; this only controls whether
    /// a finished gesture is allowed to add a glide on top of it. Takes effect
    /// at runtime, without a config reload.
    pub fn set_wheel_glide(&self, enabled: bool) {
        self.scroll.set_wheel_glide_enabled(enabled);
    }

    pub fn wheel_glide_enabled(&self) -> bool {
        self.scroll.wheel_glide_enabled()
    }

    /// Advance the pending wheel gesture to `now`, arming its glide if the
    /// gesture has gone idle and was significant. Returns whether a glide was
    /// armed.
    ///
    /// `now` is a parameter rather than a clock read inside so the whole
    /// sequence is reproducible under an injected instant. Call once per
    /// rendered frame, alongside [`Self::advance_scroll_animation`], for as
    /// long as [`Self::scroll_needs_frames`] holds.
    pub fn advance_wheel_glide(&self, now: std::time::Instant) -> bool {
        self.scroll.advance_wheel_glide(now)
    }

    /// Advance the cursor-follow inertia to `now`, arming the carry when a
    /// gesture has gone idle and travelled far enough. Returns whether a carry
    /// was armed.
    ///
    /// `now` is a parameter rather than a clock read inside so the whole
    /// sequence is reproducible under an injected instant, exactly as for
    /// [`Self::advance_wheel_glide`]. Call once per rendered frame, alongside
    /// [`Self::advance_scroll_animation`] and [`Self::advance_wheel_glide`], for
    /// as long as [`Self::scroll_needs_frames`] holds. It never arms a carry
    /// while a tween is in flight, so it must run *after* the tween has been
    /// sampled.
    pub fn advance_scrolloff_inertia(&self, now: std::time::Instant) -> bool {
        self.scroll.advance_scrolloff_inertia(now)
    }

    pub fn visible_visual_range(&self) -> (usize, usize) {
        let position = self.scroll_position();
        let viewport = self.scroll.viewport_size();
        let first_row = self.scroll.pixels_to_anchor(position.y);
        let last_row = self
            .scroll
            .pixels_to_anchor(position.y + viewport.height)
            .saturating_add(1)
            .min(self.content_visual_rows());

        (first_row, last_row)
    }
}

fn editor_viewport_content_metrics(
    metrics_cache: &mut EditorDocumentMetricsCache,
    document: &Document,
    view: &helix_view::View,
    layout: EditorViewportContentLayout<'_>,
) -> (u16, EditorDocumentMetrics) {
    let gutter_columns = view
        .gutter_offset(document)
        .saturating_add(layout.extra_gutter_columns);
    let metrics = metrics_cache.resolve(EditorDocumentMetricsCacheResolveParams {
        document,
        view,
        theme: layout.theme,
        bounds: layout.bounds,
        gutter_columns,
        cell_width: layout.cell_width,
        minimum_columns: layout.minimum_columns,
    });

    (gutter_columns, metrics)
}

fn editor_viewport_surface_metrics(
    metrics_cache: &mut EditorDocumentMetricsCache,
    document: &Document,
    view: &helix_view::View,
    layout: EditorViewportContentLayout<'_>,
    visible_rows: usize,
) -> (u16, EditorDocumentMetrics) {
    let (current_gutter_columns, current_metrics) =
        editor_viewport_content_metrics(metrics_cache, document, view, layout);
    let mut surface_view = view.clone();
    surface_view.area =
        helix_view_area_for_surface(0, current_metrics.viewport_columns, visible_rows);
    let surface_gutter_columns = surface_view
        .gutter_offset(document)
        .saturating_add(layout.extra_gutter_columns);

    if surface_gutter_columns == current_gutter_columns {
        return (current_gutter_columns, current_metrics);
    }

    let metrics = metrics_cache.resolve(EditorDocumentMetricsCacheResolveParams {
        document,
        view: &surface_view,
        theme: layout.theme,
        bounds: layout.bounds,
        gutter_columns: surface_gutter_columns,
        cell_width: layout.cell_width,
        minimum_columns: layout.minimum_columns,
    });

    (surface_gutter_columns, metrics)
}

fn apply_helix_view_area_plan(
    view: &mut helix_view::View,
    plan: EditorViewportViewAreaPlan,
) -> bool {
    if !plan.changed {
        return false;
    }

    trace!(
        old_area = ?plan.previous_area,
        new_area = ?plan.target_area,
        "Syncing native viewport dimensions to Helix view area"
    );
    view.area = plan.target_area;
    true
}

pub fn helix_view_area_plan_for_surface(
    previous_area: Rect,
    gutter_columns: u16,
    viewport_columns: u16,
    visible_rows: usize,
) -> EditorViewportViewAreaPlan {
    let target_area = helix_view_area_for_surface(gutter_columns, viewport_columns, visible_rows);

    EditorViewportViewAreaPlan {
        previous_area,
        target_area,
        changed: previous_area != target_area,
    }
}

fn helix_view_area_for_surface(
    gutter_columns: u16,
    viewport_columns: u16,
    visible_rows: usize,
) -> Rect {
    let width = gutter_columns.saturating_add(viewport_columns).max(1);
    let height = u16::try_from(visible_rows.saturating_add(1))
        .unwrap_or(u16::MAX)
        .max(1);

    Rect::new(0, 0, width, height)
}

pub fn editor_viewport_size_for_bounds(bounds: Bounds<Pixels>) -> Size<Pixels> {
    size(
        bounds.size.width,
        (bounds.size.height - px(1.0)).max(px(0.0)),
    )
}

fn editor_text_viewport_size_for_bounds(
    bounds: Bounds<Pixels>,
    gutter_columns: u16,
    cell_width: Pixels,
) -> Size<Pixels> {
    let geometry = crate::EditorSurfaceGeometry::new(bounds, gutter_columns, cell_width);
    let text_bounds = geometry.text_bounds();

    size(
        text_bounds.size.width.max(px(0.0)),
        (bounds.size.height - px(1.0)).max(px(0.0)),
    )
}

fn viewport_conversion_base_key(
    document: &Document,
    view_id: ViewId,
    text_format: &TextFormat,
) -> EditorViewportConversionBaseKey {
    EditorViewportConversionBaseKey {
        document_version: document.version(),
        text_len: document.text().len_chars(),
        view_id,
        text_format: EditorViewportTextFormatKey::from(text_format),
    }
}

fn helix_viewport_snapshot_for_synced_plan(
    document: &Document,
    plan: EditorViewportViewPositionPlan,
) -> HelixViewportSnapshot {
    let text = document.text();
    let anchor = plan.view_position.anchor.min(text.len_chars());

    HelixViewportSnapshot {
        anchor_line: text.char_to_line(anchor),
        vertical_offset: plan.view_position.vertical_offset,
        top_visual_row: plan.top_visual_row,
    }
}

pub fn helix_viewport_snapshot(
    text: RopeSlice<'_>,
    view_offset: ViewPosition,
    text_format: &TextFormat,
    annotations: &TextAnnotations<'_>,
) -> HelixViewportSnapshot {
    let anchor = view_offset.anchor.min(text.len_chars());
    let anchor_line = text.char_to_line(anchor);
    let anchor_visual_row = visual_offset_from_block(text, 0, anchor, text_format, annotations)
        .0
        .row;
    let top_visual_row = anchor_visual_row.saturating_add(view_offset.vertical_offset);

    HelixViewportSnapshot {
        anchor_line,
        vertical_offset: view_offset.vertical_offset,
        top_visual_row,
    }
}

pub fn view_position_for_top_visual_row(
    text: RopeSlice<'_>,
    top_visual_row: usize,
    horizontal_offset: usize,
    text_format: &TextFormat,
    annotations: &TextAnnotations<'_>,
) -> ViewPosition {
    let (anchor, vertical_offset) = char_idx_at_visual_offset(
        text,
        0,
        isize::try_from(top_visual_row).unwrap_or(isize::MAX),
        0,
        text_format,
        annotations,
    );

    ViewPosition {
        anchor,
        vertical_offset,
        horizontal_offset: if text_format.soft_wrap {
            0
        } else {
            horizontal_offset
        },
    }
}

pub fn view_position_plan_for_top_visual_row(
    text: RopeSlice<'_>,
    previous_view_position: ViewPosition,
    top_visual_row: usize,
    horizontal_offset: usize,
    text_format: &TextFormat,
    annotations: &TextAnnotations<'_>,
) -> EditorViewportViewPositionPlan {
    let view_position = view_position_for_top_visual_row(
        text,
        top_visual_row,
        horizontal_offset,
        text_format,
        annotations,
    );

    EditorViewportViewPositionPlan {
        top_visual_row,
        previous_view_position,
        view_position,
        changed: previous_view_position != view_position,
    }
}

fn apply_view_position_plan(
    document: &mut Document,
    view_id: ViewId,
    plan: EditorViewportViewPositionPlan,
) -> bool {
    if !plan.changed {
        return false;
    }

    document.set_view_offset(view_id, plan.view_position);
    true
}

pub fn document_cursor_visual_row(
    document: &Document,
    view: &helix_view::View,
    view_id: ViewId,
    text_format: &TextFormat,
) -> usize {
    let text = document.text().slice(..);
    let cursor_char_idx = document.selection(view_id).primary().cursor(text);
    document_cursor_visual_row_for_cursor(document, view, text_format, cursor_char_idx)
}

fn document_cursor_visual_row_for_cursor(
    document: &Document,
    view: &helix_view::View,
    text_format: &TextFormat,
    cursor_char_idx: usize,
) -> usize {
    let text = document.text().slice(..);
    let cursor_at_trailing_newline = cursor_char_idx == text.len_chars()
        && text.len_chars() > 0
        && text.char(text.len_chars() - 1) == '\n';
    let annotations = view.text_annotations(document, None);

    if cursor_at_trailing_newline {
        return if text_format.soft_wrap {
            soft_wrap_visual_position(text, text_format, Some(&annotations), 0, cursor_char_idx)
                .map(|position| position.visual_line)
                .unwrap_or_else(|| text.len_lines().saturating_sub(1))
        } else {
            text.len_lines().saturating_sub(1)
        };
    }

    visual_offset_from_block(text, 0, cursor_char_idx, text_format, &annotations)
        .0
        .row
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use arc_swap::{ArcSwap, access::Map};
    use gpui::{Bounds, point, px, size};
    use helix_core::{
        Rope, Selection, Transaction, doc_formatter::TextFormat, syntax,
        text_annotations::TextAnnotations,
    };
    use helix_view::{
        Document, DocumentId, Editor, View,
        editor::Action,
        editor::{Config, GutterConfig},
        graphics::Rect,
        handlers::Handlers,
        theme,
        view::ViewPosition,
    };

    use crate::scroll_manager::key_inertia::OVERSHOOT_DURATION;

    use super::*;

    fn default_annotations() -> TextAnnotations<'static> {
        TextAnnotations::default()
    }

    fn test_document_and_view(text: &str) -> (Document, View) {
        let config = Arc::new(ArcSwap::new(Arc::new(Config::default())));
        let syntax_loader = Arc::new(ArcSwap::from_pointee(syntax::Loader::default()));
        let mut document = Document::from(Rope::from(text), None, config, syntax_loader);
        let view = View::new(DocumentId::default(), GutterConfig::default());
        document.ensure_view_init(view.id);

        (document, view)
    }

    fn test_handlers() -> Handlers {
        let (completion_tx, _) = tokio::sync::mpsc::channel(1);
        let (signature_tx, _) = tokio::sync::mpsc::channel(1);
        let (auto_save_tx, _) = tokio::sync::mpsc::channel(1);
        let (doc_colors_tx, _) = tokio::sync::mpsc::channel(1);
        let (doc_links_tx, _) = tokio::sync::mpsc::channel(1);
        let (pull_diagnostics_tx, _) = tokio::sync::mpsc::channel(1);
        let (pull_all_diagnostics_tx, _) = tokio::sync::mpsc::channel(1);
        let (code_action_hint_tx, _) = tokio::sync::mpsc::channel(1);

        Handlers {
            completions: helix_view::handlers::completion::CompletionHandler::new(completion_tx),
            signature_hints: signature_tx,
            auto_save: auto_save_tx,
            document_colors: doc_colors_tx,
            document_links: doc_links_tx,
            word_index: helix_view::handlers::word_index::Handler::spawn(),
            pull_diagnostics: pull_diagnostics_tx,
            pull_all_documents_diagnostics: pull_all_diagnostics_tx,
            code_action_hint: code_action_hint_tx,
        }
    }

    fn test_editor_with_text(text: &str) -> (Editor, DocumentId, ViewId) {
        let config = Arc::new(ArcSwap::new(Arc::new(Config::default())));
        let syntax_loader = Arc::new(ArcSwap::from_pointee(syntax::Loader::default()));
        let theme_loader = Arc::new(theme::Loader::new(&[]));
        let mut editor = Editor::new(
            Rect::new(0, 0, 80, 24),
            theme_loader,
            syntax_loader,
            Arc::new(Map::new(Arc::clone(&config), |config: &Config| config)),
            test_handlers(),
            helix_loader::workspace_trust::WorkspaceTrust::fully_trusted(),
        );
        let doc_id = editor.new_file(Action::VerticalSplit);
        let view_id = editor.tree.focus;
        let doc = editor.document_mut(doc_id).unwrap();
        let transaction = Transaction::change(doc.text(), [(0, 0, Some(text.into()))].into_iter());
        doc.apply(&transaction, view_id);

        (editor, doc_id, view_id)
    }

    /// The engine-off wheel path is still exactly 1:1 and instant.
    ///
    /// This fixture never enables `smooth_scrolling`, and a smoothed notch *is* a
    /// tween, so there is nothing to ease with and the notch is applied directly.
    /// The test is therefore also the regression guard for that fallback: if the
    /// wheel ever armed a tween with the engine off, the position would still read
    /// `0px` here (a tween has not moved yet) and the update would report row 2.
    #[test]
    fn viewport_reports_subrow_wheel_scroll() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(400.0)), 100);
        assert!(!viewport.smooth_scrolling_enabled());

        let update = viewport.scroll_by_delta(point(px(0.0), px(-5.0)));

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 0);
        assert_eq!(update.top_visual_row, 0);
        assert_eq!(update.offset_within_row, px(5.0));
        assert_eq!(viewport.scroll_position().y, px(5.0));
        assert!(!viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_reports_crossed_visual_rows() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(400.0)), 100);

        let update = viewport.scroll_by_delta(point(px(0.0), px(-25.0)));

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 1);
        assert_eq!(update.top_visual_row, 1);
        assert_eq!(update.offset_within_row, px(5.0));
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_scroll_request_moves_by_visual_rows() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);

        let update = viewport.apply_scroll_request(EditorViewportScrollRequest::VisualRows(3));

        assert!(update.changed);
        assert_eq!(viewport.scroll_position().y, px(60.0));
        assert_eq!(update.crossed_visual_rows, 3);
        assert_eq!(update.top_visual_row, 3);
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_scroll_request_clamps_above_document_start() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);
        viewport.apply_scroll_request(EditorViewportScrollRequest::VisualRows(2));

        let update = viewport.apply_scroll_request(EditorViewportScrollRequest::VisualRows(-10));

        assert!(update.changed);
        assert_eq!(viewport.scroll_position().y, px(0.0));
        assert_eq!(update.crossed_visual_rows, -2);
        assert_eq!(update.top_visual_row, 0);
    }

    #[test]
    fn viewport_page_scroll_request_moves_by_visible_rows() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);

        let update = viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));

        assert!(update.changed);
        assert_eq!(viewport.visible_visual_rows(), 5);
        assert_eq!(viewport.scroll_position().y, px(100.0));
        assert_eq!(update.crossed_visual_rows, 5);
        assert_eq!(update.top_visual_row, 5);
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_page_fraction_request_moves_by_fractional_visible_rows() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);

        let update =
            viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPageFraction {
                pages: 1,
                divisor: 2,
            });

        assert!(update.changed);
        assert_eq!(viewport.visible_visual_rows(), 5);
        assert_eq!(viewport.scroll_position().y, px(40.0));
        assert_eq!(update.crossed_visual_rows, 2);
        assert_eq!(update.top_visual_row, 2);
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_smooth_scrolling_is_disabled_by_default() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);

        assert!(!viewport.smooth_scrolling_enabled());

        let update = viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));

        assert!(update.changed);
        assert_eq!(viewport.scroll_position().y, px(100.0));
        assert!(!viewport.scroll_animation_active());
    }

    #[test]
    fn viewport_smooth_scroll_reports_destination_before_the_tween_moves() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);
        let mut instant = EditorViewport::new(px(20.0));
        instant.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);

        viewport.set_smooth_scrolling(true);
        assert!(viewport.smooth_scrolling_enabled());

        let update = viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));

        // The destination has to be reported even though the tween has not moved
        // the viewport yet, otherwise no repaint is ever scheduled.
        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 5);
        assert_eq!(update.top_visual_row, 5);
        assert_eq!(update.offset_within_row, px(0.0));
        assert_eq!(viewport.scroll_position().y, px(0.0));
        assert!(viewport.scroll_animation_active());

        // Driving the tween past its duration lands exactly where the instant
        // path would have put the viewport.
        assert!(viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500)));
        assert_eq!(viewport.scroll_position().y, px(100.0));
        assert_eq!(viewport.top_visual_row(), 5);
        assert!(!viewport.scroll_animation_active());

        instant.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));
        assert_eq!(viewport.scroll_position(), instant.scroll_position());
    }

    #[test]
    fn viewport_smooth_scroll_moves_continuously_toward_the_destination() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);
        viewport.set_smooth_scrolling(true);

        let update = viewport.apply_scroll_request(EditorViewportScrollRequest::VisualRows(3));
        let start = Instant::now();
        assert_eq!(update.top_visual_row, 3);
        assert_eq!(viewport.scroll_position().y, px(0.0));

        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(40)));
        let mid = viewport.scroll_position().y;
        assert!(mid > px(0.0));
        assert!(mid < px(60.0));

        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(1000)));
        assert_eq!(viewport.scroll_position().y, px(60.0));
        assert!(!viewport.scroll_animation_active());
    }

    #[test]
    fn viewport_scroll_needs_frames_tracks_tween_liveness() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);

        assert!(!viewport.scroll_needs_frames());

        viewport.set_smooth_scrolling(true);
        assert!(!viewport.scroll_needs_frames());

        viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));

        // Frames are needed from the moment the tween is armed, even before any
        // frame has moved a pixel.
        assert!(viewport.scroll_needs_frames());
        assert!(viewport.scroll_animation_active());

        assert!(viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500)));

        assert!(!viewport.scroll_needs_frames());
        assert!(!viewport.scroll_animation_active());
    }

    /// A `Scrolloff` fixture big enough for the eased path to be measurable.
    ///
    /// 800x800 bounds at a 20px line height give
    /// `visible_visual_rows() = floor(800 / 20) = 40`. With `scrolloff = 5` that
    /// is a band of rows `origin + 5 ..= origin + 35`, and the snap threshold is
    /// `visible_rows - margin = 35` rows of travel.
    ///
    /// `content_visual_rows` is the *total* row count, so the scroll range is
    /// `20 * content_visual_rows - 800` px. The callers below assert the specific
    /// target row they need is unclamped rather than trusting the arithmetic here.
    fn scrolloff_reveal_viewport(content_visual_rows: usize) -> EditorViewport {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(
            px(20.0),
            size(px(800.0), px(800.0)),
            content_visual_rows,
        );
        viewport.set_smooth_scrolling(true);
        assert_eq!(
            viewport.visible_visual_rows(),
            40,
            "test fixture drifted: every expected row below assumes 40 visible rows"
        );
        // The scroll range is `20 * content_visual_rows - 800` px, and every
        // target row below is quoted against it, so pin the derivation rather
        // than let a silent re-clamp masquerade as a correct landing row.
        assert_eq!(
            viewport.max_scroll_offset().height,
            px((20.0 * content_visual_rows as f32 - 800.0).max(0.0)),
            "test fixture drifted: every expected pixel below is derived from this range"
        );
        viewport
    }

    /// Arm the 40-row page tween from an explicit origin clock.
    ///
    /// `editor_jump_duration(40) = min(60 + 6 * 40, 280) = 280ms`, so the flight is
    /// `0px -> 800px` and the destination is row 40. Arming through
    /// `animate_scroll_to_at` rather than `apply_scroll_request` is what lets the
    /// samples below be exact arithmetic instead of a race against the wall clock;
    /// it is the same tween `VisualPages(1)` produces on a 40-row viewport.
    fn arm_page_tween_from(viewport: &EditorViewport, origin: Instant) {
        viewport.scroll.animate_scroll_to_at(
            point(px(0.0), px(800.0)),
            editor_jump_duration(40),
            origin,
        );
        assert!(viewport.scroll_animation_active());
    }

    /// The `Scrolloff` band is measured from the tween's **destination**, and a
    /// target that *is* chosen **retargets** the tween instead of cancelling it.
    ///
    /// This is the traced `page_down` case, pinned as correct behaviour rather
    /// than characterised as a failure. The trace: a page scroll arms a tween to
    /// row 40, `sync_cursor_after_native_page_scroll` moves the Helix cursor into
    /// the *destination* page at `40 + scrolloff = 45`, and the next painted frame
    /// applies a `Scrolloff` reveal for row 45 while the tween is still in flight.
    ///
    /// Fixture: 800x800 bounds, 20px rows, 100 content rows, `scrolloff = 5`, so
    /// `visible_visual_rows() = 40` and the page tween runs `0px -> 800px` over
    /// `editor_jump_duration(40) = 280ms`. The scroll range is
    /// `20 * 100 - 800 = 1200px`, so row 40 is unclamped.
    ///
    /// The 20ms sample is the frame the traced bug landed on:
    ///  - `t = 20 / 280 = 0.07143`
    ///  - `ease_out_quad(t) = 2t - t^2 = 0.14286 - 0.00510 = 0.13776`
    ///  - `y = 0 + 800 * 0.13776 = 110.20px` -> live top row `floor(110.20 / 20) = 5`
    ///
    /// Band arithmetic for cursor row 45:
    ///  - From the destination: `lower = 40 + 5 = 45`, `upper = 40 + (40 - 5) = 75`.
    ///    `45 < 45` is false and `45 >= 75` is false, so the cursor is already
    ///    visible and the correct answer is *no motion at all*.
    ///  - From the mid-tween row 5: `lower = 10`, `upper = 5 + 35 = 40`, so
    ///    `45 >= 40` fires and the target becomes
    ///    `45 + 5 + 1 - 40 = 11` — 29 rows short of the destination the tween is
    ///    flying towards. That is the whole bug, and this test is what rules it out.
    #[test]
    fn reveal_band_uses_the_tween_destination_and_retargets_instead_of_cancelling() {
        // --- the traced case: the reveal must not move anything -----------------
        let viewport = scrolloff_reveal_viewport(100);
        let origin = Instant::now();
        arm_page_tween_from(&viewport, origin);

        assert!(viewport.advance_scroll_animation_at(origin + Duration::from_millis(20)));
        assert_eq!(
            viewport.top_visual_row(),
            5,
            "the 20ms sample must land mid-tween on row 5, not on the destination"
        );

        // 45 is where the cursor really is: destination (40) + scrolloff (5).
        //
        // `changed` is destination-based by design, so it is `true` whenever a
        // tween is in flight even if the reveal itself moves nothing: it answers
        // "does this request have a destination", which is what the surface layer
        // needs in order to know a paint armed motion. Asserting on it here would
        // be asserting the wrong thing. The intent is that the reveal neither
        // moved the viewport nor retargeted the tween, so assert those directly.
        let sampled = viewport.scroll_position();
        let reveal = viewport.reveal_visual_row(45, EditorCursorReveal::Scrolloff, 5);

        assert_eq!(
            reveal.top_visual_row,
            40,
            "the update reports the destination, which is still row 40"
        );
        assert_eq!(
            viewport.scroll_position(),
            sampled,
            "row 45 is inside the destination band 45..=75, so the reveal must not move anything"
        );
        assert!(
            viewport.scroll_animation_active(),
            "the reveal took down the tween it was applied on top of"
        );
        assert!(
            (px(100.0)..px(120.0)).contains(&viewport.scroll_position().y),
            "the tween origin moved: {} is not the sampled 110.20px",
            viewport.scroll_position().y
        );

        // And the page still lands where it was always going to land.
        assert!(viewport.advance_scroll_animation_at(origin + Duration::from_millis(280)));
        assert_eq!(viewport.scroll_position().y, px(800.0));
        assert_eq!(viewport.top_visual_row(), 40);
        assert!(!viewport.scroll_animation_active());

        // --- the retarget case: a real target replaces the flight ---------------
        // 200 rows this time, so a cursor well past the destination is legal:
        // `max_scroll_offset().height = 20 * 200 - 800 = 3200px`, and the retarget
        // below aims at row 66 = 1320px.
        let viewport = scrolloff_reveal_viewport(200);
        let origin = Instant::now();
        arm_page_tween_from(&viewport, origin);
        assert!(viewport.advance_scroll_animation_at(origin + Duration::from_millis(20)));
        assert_eq!(viewport.top_visual_row(), 5);

        // Cursor row 100 against the destination band `45..=75`: `100 >= 75` fires,
        // so the target is `100 + 5 + 1 - 40 = 66`. `travel_rows` is measured from
        // the same origin the band was, `66 - 40 = 26`, and
        // `26 <= visible_rows - margin = 35`, so this one eases.
        let before = Instant::now();
        let reveal = viewport.reveal_visual_row(100, EditorCursorReveal::Scrolloff, 5);
        let after = Instant::now();

        assert!(reveal.changed);
        assert_eq!(reveal.top_visual_row, 66, "66 = 100 + 5 + 1 - 40");
        assert_eq!(
            reveal.crossed_visual_rows,
            61,
            "61 = 66 - 5, from the live row to the destination"
        );
        assert_eq!(
            editor_jump_duration(26),
            Duration::from_millis(216),
            "the duration moved; the samples below assume 60 + 6 * 26"
        );
        assert!(
            viewport.scroll_animation_active(),
            "the retarget cancelled instead of replacing the page tween"
        );
        assert!(
            (px(100.0)..px(120.0)).contains(&viewport.scroll_position().y),
            "the retarget teleported to its target instead of easing from 110.20px: {}",
            viewport.scroll_position().y
        );

        // The replacement is a *fresh, full* 216ms flight from the live position,
        // not the 260ms the old tween had left. At 100ms the fresh tween is at
        // `t = 100/216 = 0.463`, `ease = 0.463 * 1.537 = 0.712`,
        // `y = 110.20 + 1209.80 * 0.712 = 971.5px`; an inherited 260ms budget would
        // be at `t = 0.385`, `y = 862.0px`. Both are strictly short of 1320px, so
        // this sample pins "still travelling" rather than the budget itself — the
        // budget is discriminated at the deadline sample below.
        assert!(viewport.advance_scroll_animation_at(before + Duration::from_millis(100)));
        assert!(viewport.scroll_animation_active(), "the retarget completed in under 100ms");
        assert!(
            (px(110.0)..px(1000.0)).contains(&viewport.scroll_position().y),
            "y = {} is outside the 110.20..=971.5px the fresh flight passes through",
            viewport.scroll_position().y
        );

        // At 216ms the fresh flight is done and lands exactly on 1320px. Had the
        // remaining 260ms been carried forward instead, `t = 216/260 = 0.831`,
        // `ease = 0.831 * 1.169 = 0.971`, and `y = 110.20 + 1209.80 * 0.971 =
        // 1284.9px` with the tween still running.
        assert!(viewport.advance_scroll_animation_at(after + Duration::from_millis(216)));
        assert_eq!(
            viewport.scroll_position().y,
            px(1320.0),
            "the retarget did not run its full 216ms from the live position"
        );
        assert_eq!(viewport.top_visual_row(), 66);
        assert!(!viewport.scroll_animation_active());
    }

    /// Applying the armed reveal and discarding it are now equally harmless.
    ///
    /// `reveal_visual_row` measures the `Scrolloff` band from the tween's
    /// destination, so the very request that used to kill the page tween — the
    /// traced `page_down` reveal for row 45 while the page tween is still at
    /// row 5 — now resolves to "already visible" and leaves the flight alone.
    ///
    /// `Workspace::handle_viewport_scroll` still calls
    /// `clear_cursor_reveal_request` after the cursor sync. That workaround is no
    /// longer load-bearing: discarding the request and applying it both leave the
    /// page tween running to row 40. Both halves are asserted because the
    /// workspace wiring cannot be driven headlessly, and this is the viewport
    /// contract it relies on either way.
    #[test]
    fn page_scroll_reveal_applied_or_discarded_both_leave_the_page_tween_intact() {
        // Applying the armed reveal. Arithmetic is spelled out in
        // `reveal_band_uses_the_tween_destination_and_retargets_instead_of_cancelling`.
        let applied = scrolloff_reveal_viewport(100);
        let origin = Instant::now();
        arm_page_tween_from(&applied, origin);

        applied.request_cursor_reveal(EditorCursorReveal::Scrolloff);
        assert!(applied.advance_scroll_animation_at(origin + Duration::from_millis(20)));
        assert_eq!(applied.top_visual_row(), 5, "mid-tween");
        let armed = applied
            .take_cursor_reveal_request()
            .expect("armed reveal from the selection change");
        // `changed` is destination-based, so it is `true` for any in-flight tween
        // regardless of what this reveal did; it reports that a paint armed
        // motion, not that the viewport moved. Assert the two things that actually
        // matter — the destination is untouched and the live position is unmoved.
        let sampled = applied.scroll_position();
        let update = applied.reveal_visual_row(45, armed, 5);

        assert_eq!(
            update.top_visual_row,
            40,
            "the tween was retargeted away from row 40"
        );
        assert_eq!(
            applied.scroll_position(),
            sampled,
            "row 45 is inside the destination band, so applying the reveal must move nothing"
        );
        assert!(applied.scroll_animation_active(), "applying the reveal killed the tween");
        assert!(applied.advance_scroll_animation_at(origin + Duration::from_millis(280)));
        assert_eq!(applied.top_visual_row(), 40);
        assert!(!applied.scroll_animation_active());

        // Discarding the same request — what `clear_cursor_reveal_request` does.
        let discarded = scrolloff_reveal_viewport(100);
        let origin = Instant::now();
        arm_page_tween_from(&discarded, origin);

        discarded.request_cursor_reveal(EditorCursorReveal::Scrolloff);
        assert_eq!(
            discarded.take_cursor_reveal_request(),
            Some(EditorCursorReveal::Scrolloff),
            "the selection change armed a reveal"
        );
        assert!(discarded.scroll_animation_active());

        assert!(discarded.advance_scroll_animation_at(origin + Duration::from_millis(280)));
        assert_eq!(discarded.top_visual_row(), 40);
        assert!(!discarded.scroll_animation_active());
    }

    /// A `Scrolloff` reveal that keeps the cursor inside the painted band eases,
    /// and the ease lands exactly on the row the band chose.
    ///
    /// Fixture: 800x800 bounds, 20px rows, 200 content rows, `scrolloff = 5`, so
    /// `visible_visual_rows() = 40` and the snap threshold is
    /// `visible_rows - margin = 35` rows. A one-row `Scrolloff` travel is what
    /// ordinary cursor following produces, and one row is nowhere near the
    /// threshold, so these are the eased cases.
    ///
    /// Forward, from row 0, cursor at 35:
    ///  - `lower = 0 + 5 = 5`, `upper = 0 + (40 - 5) = 35`, so `35 >= 35` fires.
    ///  - target `= 35 + 5 + 1 - 40 = 1`, `travel_rows = 1 - 0 = 1`,
    ///    `1 <= 35`, so the reveal eases over
    ///    `editor_jump_duration(1) = 60 + 6 = 66ms` towards `1 * 20 = 20px`.
    ///
    /// Backward, from row 10 (reached instantly through the scrollbar, which is
    /// the cancel-on-purpose path and so is a clean idle origin), cursor at 14:
    ///  - `lower = 10 + 5 = 15`, so `14 < 15` fires.
    ///  - target `= 14 - 5 = 9`, `travel_rows = 9 - 10 = -1`, `1 <= 35`, so it
    ///    eases over the same 66ms towards `9 * 20 = 180px`.
    #[test]
    fn scrolloff_reveal_within_the_painted_band_is_eased_and_lands_on_target() {
        assert_eq!(
            editor_jump_duration(1),
            Duration::from_millis(66),
            "the duration moved; the 66ms samples below assume 60 + 6 * 1"
        );

        // Forward.
        let forward = scrolloff_reveal_viewport(200);
        let update = forward.reveal_visual_row(35, EditorCursorReveal::Scrolloff, 5);

        assert!(update.changed);
        assert_eq!(update.top_visual_row, 1, "1 = 35 + 5 + 1 - 40");
        assert_eq!(update.crossed_visual_rows, 1);
        assert_eq!(update.offset_within_row, px(0.0));
        assert!(forward.scroll_animation_active(), "a one-row reveal must ease");
        assert_eq!(
            forward.scroll_position().y,
            px(0.0),
            "an armed tween has not moved the viewport yet"
        );
        assert_eq!(forward.top_visual_row(), 0);

        assert!(forward.advance_scroll_animation_at(Instant::now() + Duration::from_millis(66)));
        assert_eq!(forward.scroll_position().y, px(20.0));
        assert_eq!(forward.top_visual_row(), 1, "the ease must land exactly on its target");
        assert!(!forward.scroll_animation_active());

        // Backward.
        let backward = scrolloff_reveal_viewport(200);
        backward.scroll_to_vertical_position_from_scrollbar(px(200.0));
        assert_eq!(backward.top_visual_row(), 10, "fixture setup");
        assert!(!backward.scroll_animation_active());

        let update = backward.reveal_visual_row(14, EditorCursorReveal::Scrolloff, 5);

        assert!(update.changed);
        assert_eq!(update.top_visual_row, 9, "9 = 14 - 5");
        assert_eq!(update.crossed_visual_rows, -1);
        assert!(backward.scroll_animation_active());
        assert_eq!(backward.scroll_position().y, px(200.0), "the ease has not moved yet");
        assert_eq!(backward.top_visual_row(), 10);

        assert!(backward.advance_scroll_animation_at(Instant::now() + Duration::from_millis(66)));
        assert_eq!(backward.scroll_position().y, px(180.0));
        assert_eq!(backward.top_visual_row(), 9);
        assert!(!backward.scroll_animation_active());
    }

    /// A `Scrolloff` travel that would strand the cursor snaps instead.
    ///
    /// The hazard is not the distance, it is the cursor leaving the painted band:
    /// a line outside the rendered range is not painted at all, so during a tween
    /// the cursor would simply vanish. The band spans `visible_rows` rows, the
    /// cursor sits at most `margin` rows inside whichever edge fired, and the view
    /// travels `travel_rows`, so the cursor leaves the far edge exactly when
    /// `travel_rows > visible_rows - margin`.
    ///
    /// Fixture: 800x800 bounds, 20px rows, 200 content rows, `scrolloff = 5`, so
    /// `visible_visual_rows() = 40` and the threshold is
    /// `visible_rows - margin = 40 - 5 = 35` rows exactly. Each case below sits on
    /// one side of that number, and the boundary itself is asserted rather than
    /// inferred.
    ///
    /// From top row `T` the band is `T + 5 ..= T + 35`, and
    ///  - forward, cursor `C >= T + 35` -> target `C + 5 + 1 - 40 = C - 34`, so
    ///    `travel = C - 34 - T`. With `T = 0`: `C = 69 -> 35` (travel 35, eased)
    ///    and `C = 70 -> 36` (travel 36, snapped).
    ///  - backward, cursor `C < T + 5` -> target `C - 5`, so
    ///    `travel = C - 5 - T`. With `T = 100`: `C = 70 -> 65` (travel -35, eased)
    ///    and `C = 69 -> 64` (travel -36, snapped).
    #[test]
    fn scrolloff_reveal_beyond_the_painted_band_snaps() {
        struct Case {
            label: &'static str,
            top_row: usize,
            cursor_row: usize,
            expected_target: usize,
            expected_eased: bool,
        }

        let cases = [
            Case {
                label: "forward at the threshold",
                top_row: 0,
                cursor_row: 69,
                expected_target: 35,
                expected_eased: true,
            },
            Case {
                label: "forward one row past the threshold",
                top_row: 0,
                cursor_row: 70,
                expected_target: 36,
                expected_eased: false,
            },
            Case {
                label: "backward at the threshold",
                top_row: 100,
                cursor_row: 70,
                expected_target: 65,
                expected_eased: true,
            },
            Case {
                label: "backward one row past the threshold",
                top_row: 100,
                cursor_row: 69,
                expected_target: 64,
                expected_eased: false,
            },
        ];

        for case in cases {
            let viewport = scrolloff_reveal_viewport(200);
            viewport.scroll_to_vertical_position_from_scrollbar(px(20.0 * case.top_row as f32));
            assert_eq!(viewport.top_visual_row(), case.top_row, "{}: fixture", case.label);
            let origin_y = viewport.scroll_position().y;

            let update = viewport.reveal_visual_row(case.cursor_row, EditorCursorReveal::Scrolloff, 5);

            assert!(update.changed, "{}: nothing moved", case.label);
            assert_eq!(
                update.top_visual_row, case.expected_target,
                "{}: target row",
                case.label
            );
            assert_eq!(
                viewport.scroll_animation_active(),
                case.expected_eased,
                "{}: eased={} at the threshold visible_rows - margin = 35",
                case.label, case.expected_eased
            );

            if case.expected_eased {
                assert_eq!(
                    viewport.scroll_position().y,
                    origin_y,
                    "{}: an armed tween has not moved the viewport yet",
                    case.label
                );
                // 35 rows is the last count below the 280ms cap:
                // `editor_jump_duration(35) = 60 + 6 * 35 = 270ms`.
                assert!(viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500)));
                assert_eq!(
                    viewport.top_visual_row(),
                    case.expected_target,
                    "{}: the ease must land exactly on its target",
                    case.label
                );
            } else {
                assert_eq!(
                    viewport.scroll_position().y,
                    px(20.0 * case.expected_target as f32),
                    "{}: a snap is instant, not a tween that has not moved yet",
                    case.label
                );
            }
        }
    }

    /// The eased `Scrolloff` path is symmetric in both directions.
    ///
    /// Starting from row 20, a cursor at 55 sits exactly on the band's firing
    /// edge `20 + (40 - 5) = 55`, and a cursor at 24 from row 21 sits just inside
    /// the lower edge `21 + 5 = 26`. Both reveals therefore ease, one downwards
    /// and one upwards, and `crossed_visual_rows` has to carry the sign.
    ///
    ///  - down: target `= 55 + 5 + 1 - 40 = 21`, `travel = +1`,
    ///    `editor_jump_duration(1) = 66ms`, destination `21 * 20 = 420px`.
    ///  - up (from the row the first ease landed on): target `= 24 - 5 = 19`,
    ///    `travel = -2`, `editor_jump_duration(2) = 60 + 12 = 72ms`, destination
    ///    `19 * 20 = 380px`.
    #[test]
    fn scrolloff_reveal_eases_in_both_directions_from_the_same_origin() {
        let viewport = scrolloff_reveal_viewport(200);
        viewport.scroll_to_vertical_position_from_scrollbar(px(400.0));
        assert_eq!(viewport.top_visual_row(), 20, "fixture setup");

        let update = viewport.reveal_visual_row(55, EditorCursorReveal::Scrolloff, 5);

        assert!(update.changed);
        assert_eq!(update.top_visual_row, 21, "21 = 55 + 5 + 1 - 40");
        assert_eq!(update.crossed_visual_rows, 1);
        assert!(viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position().y, px(400.0), "the ease has not moved yet");

        assert!(viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(66)));
        assert_eq!(viewport.top_visual_row(), 21);
        assert_eq!(viewport.scroll_position().y, px(420.0));
        assert!(!viewport.scroll_animation_active());

        let update = viewport.reveal_visual_row(24, EditorCursorReveal::Scrolloff, 5);

        assert!(update.changed);
        assert_eq!(update.top_visual_row, 19, "19 = 24 - 5");
        assert_eq!(update.crossed_visual_rows, -2, "two rows up from row 21");
        assert!(viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position().y, px(420.0), "the ease has not moved yet");
        assert_eq!(viewport.top_visual_row(), 21);

        assert!(viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(72)));
        assert_eq!(viewport.top_visual_row(), 19);
        assert_eq!(viewport.scroll_position().y, px(380.0));
        assert!(!viewport.scroll_animation_active());
    }

    /// Regression: a Helix horizontal-offset sync must not cancel an in-flight
    /// vertical scroll tween.
    ///
    /// `sync_from_helix_horizontal_offset` passed `current.y` straight into
    /// `set_scroll_position_from_view_sync_preserving_subrow_offset`, which
    /// cancelled unconditionally.
    /// Because the incoming
    /// line always equalled the current line, that call could not change the
    /// vertical position at all — its only effect on a tween was to destroy it.
    /// `page_down` therefore animated a single frame and stopped partway.
    ///
    /// The sibling case, a *vertical* sync that also resolves to the same row,
    /// is covered by `same_row_helix_vertical_sync_does_not_cancel_an_in_flight_tween`.
    #[test]
    fn helix_horizontal_sync_does_not_cancel_an_in_flight_vertical_tween() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(400.0), px(800.0)), 100);
        viewport.set_content_width(px(2000.0));
        viewport.set_cell_width(px(10.0));
        viewport.set_smooth_scrolling(true);

        // 40 rows -> 800px -> the 280ms duration cap.
        let update = viewport.scroll_by_visual_rows(40);
        assert_eq!(update.top_visual_row, 40);
        assert!(viewport.scroll_animation_active());

        // Sample at half the flight, which reads ~600px under ease_out_quad:
        // y is strictly inside the travel, so the tween is demonstrably mid-air.
        assert!(viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(140)));
        let mid_position = viewport.scroll_position();
        assert!(mid_position.y > px(0.0), "the tween never advanced");
        assert!(
            mid_position.y < px(800.0),
            "the tween had already landed at {mid_position:?}"
        );
        assert!(viewport.scroll_animation_active());

        let no_soft_wrap = TextFormat {
            soft_wrap: false,
            viewport_width: 40,
            ..TextFormat::default()
        };
        viewport.sync_from_helix_horizontal_offset(7, &no_soft_wrap);

        assert_eq!(viewport.scroll_position().x, px(70.0), "x was not updated");
        assert!(
            viewport.scroll_animation_active(),
            "the horizontal sync cancelled the vertical tween"
        );
        assert_eq!(
            viewport.scroll_position().y,
            mid_position.y,
            "the horizontal sync moved the vertical position"
        );

        // And the tween is still live: it can still be driven to its destination.
        assert!(viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500)));
        assert_eq!(viewport.scroll_position().y, px(800.0));
        assert!(!viewport.scroll_animation_active());
    }

    /// Regression: a same-row Helix *vertical* view sync must not cancel an
    /// in-flight vertical scroll tween.
    ///
    /// Prevents the traced `page_down` failure: the 228ms tween lived 2.85ms and
    /// covered 2.5px of 728px because a sync arrived with `current_row = 0
    /// incoming_row = 0`. That sync resolved `y` to `current.y` — it moved
    /// nothing vertically — and still cancelled the flight, because the cancel
    /// used to sit above the `preserved_subrow` test.
    ///
    /// Sample arithmetic (one row at 20px => 80px viewport, 100 content rows, so
    /// 1200px of travel and no clamping):
    ///  - `scroll_by_visual_rows(1)` arms 0 -> 20px over `editor_jump_duration(1)`
    ///    = 60 + 6 = 66ms.
    ///  - At 16ms, `t = 16/66 = 0.2424`, `ease_out_quad(t) = 2t - t^2 =
    ///    0.4848 - 0.0588 = 0.4261`, so `y = 20 * 0.4261 = 8.52px`.
    ///  - `8.52 < 20`, so the sample is still *inside* row 0: the live top row and
    ///    Helix's row 0 are the same row, which is what makes the sync a same-row
    ///    no-op. (The exact sample is not asserted — only the one-sided bounds
    ///    that must hold: it must be above 0 and below the row boundary.)
    #[test]
    fn same_row_helix_vertical_sync_does_not_cancel_an_in_flight_tween() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(400.0), px(800.0)), 100);
        viewport.set_smooth_scrolling(true);

        // One row down: 0 -> 20px over 66ms.
        let update = viewport.scroll_by_visual_rows(1);
        assert_eq!(update.top_visual_row, 1);
        assert!(viewport.scroll_animation_active());

        let start = Instant::now();
        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(16)));
        let sub_row = viewport.scroll_position();

        // Guards the arithmetic in the doc comment: the sample has to be mid-tween
        // *and* sub-row, or this test is not exercising the same-row branch.
        assert!(
            viewport.scroll_animation_active(),
            "the tween completed before the sample"
        );
        assert!(
            sub_row.y > px(0.0) && sub_row.y < px(20.0),
            "sample {sub_row:?} did not land inside row 0"
        );
        assert_eq!(viewport.top_visual_row(), 0);

        // Helix's stored row is 0, the same row the live tween is inside, so this
        // sync has no vertical effect to defend and must leave the tween alone.
        viewport.sync_from_helix_top_visual_row(0);

        assert!(
            viewport.scroll_animation_active(),
            "a same-row view sync cancelled the tween"
        );
        assert_eq!(
            viewport.scroll_position(),
            sub_row,
            "the same-row view sync moved the scroll position"
        );

        // And the spared tween is live, not merely still flagged: it can still be
        // driven the rest of the way to row 1.
        assert!(viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500)));
        assert_eq!(viewport.scroll_position().y, px(20.0));
        assert_eq!(viewport.top_visual_row(), 1);
        assert!(!viewport.scroll_animation_active());
    }

    /// The guard on the fix above: a vertical view sync that *does* change the
    /// top row still wins over an in-flight tween.
    ///
    /// A lazy "just delete the cancel" fix passes the test above and breaks this
    /// one: the tween would keep flying to row 4 while the viewport is required
    /// to sit on Helix's row 0, and the crossing-gated sync would then report a
    /// top row the cursor is nowhere near. Helix owns the top row; the tween is
    /// only ever an animation of it.
    ///
    /// Sample arithmetic (four rows at 20px):
    ///  - `scroll_by_visual_rows(4)` arms 0 -> 80px over `editor_jump_duration(4)`
    ///    = 60 + 24 = 84ms.
    ///  - At 30ms, `t = 30/84 = 0.3571`, `ease_out_quad(t) = 0.7143 - 0.1276 =
    ///    0.5867`, so `y = 80 * 0.5867 = 46.94px`.
    ///  - `46.94 > 40`, i.e. row 2: a row boundary has been crossed, so the live
    ///    top row is not 0 and the incoming row 0 is a *different* row. Reaching
    ///    row 3 would need `ease_out_quad(t) = 0.75`, i.e. `t = 0.5` and 42ms, and
    ///    row 3 would still satisfy the assertion — hence `> 0` rather than an
    ///    exact row, with the mid-tween check below as the upper bound.
    #[test]
    fn row_changing_helix_vertical_sync_still_cancels_an_in_flight_tween() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(400.0), px(800.0)), 100);
        viewport.set_smooth_scrolling(true);

        // Four rows down: 0 -> 80px over 84ms.
        let update = viewport.scroll_by_visual_rows(4);
        assert_eq!(update.top_visual_row, 4);
        assert!(viewport.scroll_animation_active());

        let start = Instant::now();
        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(30)));
        let past_crossing = viewport.scroll_position();

        // Guards the arithmetic in the doc comment: past a row crossing *and*
        // still mid-tween, so the sync below is a genuine row correction.
        assert!(
            viewport.scroll_animation_active(),
            "the tween completed before the sample"
        );
        assert!(
            past_crossing.y > px(40.0),
            "sample {past_crossing:?} had not crossed the row 1 boundary"
        );
        assert!(
            viewport.top_visual_row() > 0,
            "the tween is still inside row 0, so the sync would be a same-row no-op"
        );

        // Helix says the top row is 0; the tween says row 2 heading for row 4.
        // Helix wins.
        viewport.sync_from_helix_top_visual_row(0);

        assert!(
            !viewport.scroll_animation_active(),
            "a row-changing view sync failed to cancel the tween"
        );
        assert_eq!(viewport.scroll_position(), point(px(0.0), px(0.0)));
        assert_eq!(viewport.top_visual_row(), 0);
    }

    /// Fixture for the wheel tests: 20px rows, a 400x800 viewport and 100
    /// content rows, so `max_scroll_offset().height = 100 * 20 - 800 = 1200px`
    /// of travel. Every wheel target below stays well inside it, so nothing is
    /// clamped by the scroll range and the assertions are pure wheel arithmetic.
    ///
    /// Both switches are on, because they are two halves of one thing: wheel
    /// smoothing *is* a tween, so it is gated on the tween engine
    /// (`smooth_scrolling`) as well as on its own key.
    ///
    /// Notches are delivered through `scroll_by_delta_at` with an injected clock
    /// so that each one arms a tween whose deadline is exactly `now + TWEEN_MS`.
    /// `scroll_by_delta` would use the real clock, and a test that then samples
    /// at `now + k` would be measuring how long the test itself took.
    ///
    /// The flip side of reading that clock *before* the event is that the idle
    /// window cannot be probed at exactly `GESTURE_IDLE_MS` off it:
    /// `record_wheel_gesture` timestamps the gesture with the real clock a moment
    /// later, so `start + 90ms` is a hair *short* of the window rather than safely
    /// past it. Every test below therefore probes the idle decision at
    /// [`IDLE_PROBE`], which is unambiguously past the window. The glide is armed
    /// from whatever instant the probe uses, so the choice does not affect the
    /// glide arithmetic — only the offsets, which are all relative to the same
    /// probe.
    fn wheel_glide_viewport() -> EditorViewport {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(400.0), px(800.0)), 100);
        viewport.set_smooth_scrolling(true);
        viewport.set_wheel_glide(true);
        viewport
    }

    /// How far past the tween clock a test probes the idle-window decision. Well
    /// clear of `GESTURE_IDLE_MS = 90ms`, and well clear of `TWEEN_MS = 80ms`, so
    /// the in-gesture tween has always landed by the time the probe is taken.
    const IDLE_PROBE: Duration = Duration::from_millis(200);

    /// A single wheel event must not glide, and the frame loop must stop.
    ///
    /// One deliberate notch is a precision movement, not a flick. A *glide* on top
    /// of it would move the viewport further than the user asked for, so the
    /// event count is a gate in its own right, independent of travel.
    ///
    /// The gesture is still *eased* — that is the whole point of the change — so
    /// what has to hold is that the ease settles and the loop ends, not that the
    /// position lands instantly.
    ///
    /// Arithmetic (one event of `delta.y = -40px` on a `0px` start, tween armed at
    /// `start`):
    ///  - the target is `0 - (-40) = 40px`, so the tween runs `0px -> 40px` over
    ///    `TWEEN_MS = 80ms`. The live position is still `0px` when the event
    ///    returns, which is the assertion that the view eases rather than jumps.
    ///  - at `start + 40ms`, `t = 0.5` and `ease_out_quad(0.5) = 0.75`, so
    ///    `y = 0 + 40 * 0.75 = 30px`. At `start + 79ms` the tween is still in
    ///    flight, and at `start + 80ms` it has landed on exactly `40px`. Pinning
    ///    both bounds is what makes the duration an assertion rather than an
    ///    assumption.
    ///  - accumulated = `-40px`, so `|accumulated| = 40 < ARM_MIN_PX = 72`, and
    ///    `events = 1 < ARM_MIN_EVENTS = 2`. Both gates fail, so no glide.
    ///  - the idle decision is probed at `start + 50ms` and `start + IDLE_PROBE`
    ///    (see the fixture). The first is unambiguously inside the 90ms window
    ///    because it is nowhere near it; the second unambiguously past it.
    ///
    /// Direction, pinned explicitly: a NEGATIVE `delta.y` is a downward notch and
    /// it moves the position UP in value, because a larger position is further
    /// down the document. `0px -> 40px` is downward. This is the assertion that
    /// catches an inverted wheel, which no distance check can see.
    ///
    /// The `!scroll_needs_frames()` assertion near the end is the load-bearing
    /// one: the gesture has to have been *dropped*, not merely declined, or the
    /// render loop would request frames forever.
    #[test]
    fn single_wheel_event_glides_nothing_and_stops_the_frame_loop() {
        let viewport = wheel_glide_viewport();
        assert!(viewport.wheel_glide_enabled());

        let start = Instant::now();
        viewport.scroll_by_delta_at(point(px(0.0), px(-40.0)), start);
        assert!(
            viewport.scroll_animation_active(),
            "a qualifying notch did not arm the in-gesture tween"
        );
        assert_eq!(
            viewport.scroll_position(),
            point(px(0.0), px(0.0)),
            "the wheel teleported instead of easing onto the target"
        );

        // Inside the idle window: no glide is armed, and the loop has to keep
        // running so the gesture can still be decided.
        assert!(!viewport.advance_wheel_glide(start + Duration::from_millis(50)));

        // Half of the 80ms tween: quad(0.5) = 0.75, so 0 + 40 * 0.75 = 30px, and
        // that is downward — a larger position means further down.
        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(40)));
        assert_eq!(viewport.scroll_position(), point(px(0.0), px(30.0)));

        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(79)));
        assert!(
            viewport.scroll_animation_active(),
            "the tween finished before TWEEN_MS = 80ms"
        );
        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(80)));
        assert_eq!(
            viewport.scroll_position(),
            point(px(0.0), px(40.0)),
            "the eased notch did not land on the 40px it asked for"
        );
        assert!(!viewport.scroll_animation_active());
        // The tween is gone, but the gesture is still waiting out its idle
        // window, so the loop is still legitimately alive.
        assert!(viewport.scroll_needs_frames());

        let arm = start + IDLE_PROBE;
        assert!(!viewport.advance_wheel_glide(arm));
        assert!(!viewport.scroll_animation_active());
        assert_eq!(
            viewport.scroll_position(),
            point(px(0.0), px(40.0)),
            "the declined gesture must leave the eased position exactly as it was"
        );
        assert!(
            !viewport.scroll_needs_frames(),
            "a below-threshold gesture left the frame loop running"
        );

        // And nothing is waiting to fire later either.
        assert!(!viewport.advance_wheel_glide(arm + Duration::from_millis(500)));
        assert!(!viewport.scroll_needs_frames());
        assert_eq!(viewport.scroll_position(), point(px(0.0), px(40.0)));
    }

    /// A multi-event gesture eases onto the accumulated travel, then arms exactly
    /// one glide of the expected size and duration.
    ///
    /// This is the `N * X` test. Four notches of 50px must settle on 200px, and
    /// the way they get there is the claim: each notch adds to the previous
    /// *target* rather than to the live position, so the total is neither 50px
    /// (only the last notch) nor anything less than 200px (the earlier notches'
    /// un-flown travel thrown away).
    ///
    /// Sign convention: a negative `delta.y` scrolls DOWN, and a larger scroll
    /// position also means further down. The target is kept in the position's
    /// sign space, converted by negating the notch, so a downward gesture eases
    /// further DOWN. Getting that negation wrong is invisible to an arithmetic
    /// check and very visible to a user, so the direction is pinned here.
    ///
    /// Arithmetic (four events of `delta.y = -50px` on a `0px` start, all armed at
    /// `start`; the view cannot have moved between them, so every tween origin is
    /// the live `0px` and only the target moves):
    ///  - target after notch `n` is `0 + 50n`, so 50, 100, 150, and finally
    ///    `4 * 50 = 200px`. The last tween runs `0px -> 200px` over
    ///    `TWEEN_MS = 80ms` and the live position is still `0px` when the fourth
    ///    event returns.
    ///  - accumulated = `-200px`, so `|accumulated| = 200 >= ARM_MIN_PX = 72`
    ///    and `events = 4 >= ARM_MIN_EVENTS = 2`: both gates pass.
    ///  - glide = `clamp(-(-200) * 0.25, ±120) = +50px`, so the tween runs
    ///    `200px -> 250px`, continuing downward, over `GLIDE_MS = 140ms`.
    ///  - at `arm + 70ms`, `t = 70/140 = 0.5` and `ease_out_quad(0.5) = 0.75`, so
    ///    `y = 200 + 50 * 0.75 = 237.5px`.
    ///  - at `arm + 139ms`, `t = 0.99286`, eased `= 0.99995`, so `y ~= 249.998px`
    ///    and the tween is still in flight. At `arm + 140ms`, `t = 1` exactly, so
    ///    it lands on `250px` and deactivates. Pinning both bounds is what makes
    ///    the duration an assertion rather than an assumption: a shorter tween
    ///    would already be gone at 139ms, a longer one still flying at 140ms.
    #[test]
    fn significant_wheel_gesture_arms_a_glide_of_the_expected_size_and_duration() {
        let viewport = wheel_glide_viewport();
        let start = Instant::now();
        let mut update = None;
        for _ in 0..4 {
            update = Some(viewport.scroll_by_delta_at(point(px(0.0), px(-50.0)), start));
        }
        // The gesture is eased, not applied: the fourth notch armed a tween and
        // the view has not moved at all.
        assert!(viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position().y, px(0.0));
        assert_eq!(
            viewport.top_visual_row(),
            0,
            "the live row moved before any frame"
        );
        // What the fourth notch *reported* is its destination: 4 * 50 = 200px is
        // row 10 exactly. A 150px target would report row 7 and a 250px one row
        // 12, so this pins the accumulated total without reading the position.
        let update = update.expect("four notches were delivered");
        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 10);
        assert_eq!(update.top_visual_row, 10);
        assert_eq!(update.offset_within_row, px(0.0));

        // Land the in-gesture tween: 79ms is still in flight, 80ms is not, and
        // it lands on exactly the accumulated 4 * 50 = 200px.
        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(79)));
        assert!(
            viewport.scroll_animation_active(),
            "the in-gesture tween finished before TWEEN_MS = 80ms"
        );
        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(80)));
        assert_eq!(
            viewport.scroll_position().y,
            px(200.0),
            "four notches of 50px did not land on 4 * 50 = 200px"
        );
        assert!(!viewport.scroll_animation_active());

        let arm = start + IDLE_PROBE;
        assert!(viewport.advance_wheel_glide(arm));
        assert!(viewport.scroll_animation_active());
        // Arming does not move anything: the glide goes on top of where the
        // gesture already eased to.
        assert_eq!(viewport.scroll_position().y, px(200.0));
        // The tween keeps the loop alive now that the gesture is gone.
        assert!(viewport.scroll_needs_frames());

        assert!(viewport.advance_scroll_animation_at(arm + Duration::from_millis(70)));
        assert!(
            (viewport.scroll_position().y - px(237.5)).abs() < px(0.01),
            "half-flight sample was {:?}, expected the quad midpoint 237.5px",
            viewport.scroll_position().y
        );
        assert!(viewport.scroll_animation_active());

        assert!(viewport.advance_scroll_animation_at(arm + Duration::from_millis(139)));
        assert!(
            viewport.scroll_animation_active(),
            "the tween finished before GLIDE_MS = 140ms"
        );

        assert!(viewport.advance_scroll_animation_at(arm + Duration::from_millis(140)));
        assert_eq!(
            viewport.scroll_position().y,
            px(250.0),
            "the glide did not land on clamp(200 * 0.25, ±120) = 50px past the eased position"
        );
        assert!(!viewport.scroll_animation_active());
        assert!(!viewport.scroll_needs_frames());
    }

    /// A direction flip mid-gesture restarts the accumulation instead of netting.
    ///
    /// Netting a down-then-up flick collapses the accumulated travel toward
    /// zero, so the gesture ends up looking insignificant (no glide at all), or
    /// worse, glides in the direction the user *started* in rather than the one
    /// they ended in. The reset means the glide follows the direction the
    /// gesture actually finished in.
    ///
    /// Sign convention: a negative `delta.y` scrolls DOWN (position increases), a
    /// positive one scrolls UP. Confirmed by
    /// `single_wheel_event_glides_nothing_and_stops_the_frame_loop`, where `-40`
    /// eases the position from `0px` to `+40px`.
    ///
    /// The gesture is parked at row 20 (`400px`) on purpose. A down-then-up flick
    /// nets to zero travel, so the post-flip glide has to be the only thing that
    /// moves the view; if the start were `0px` the post-flip glide would point
    /// *up* and be clamped away at the document top, leaving nothing observable to
    /// assert.
    ///
    /// Two accumulators are in play and they behave differently on a flip, which
    /// is the point of walking through both.
    ///
    /// The **glide accumulator** lives in the wheel's sign space and is
    /// `record_wheel_gesture`'s business:
    ///  - after the first two events: `-200px`, `events = 2`.
    ///  - the third event flips the sign, so the accumulator is dropped and
    ///    restarted: `+100px`, `events = 1`. Netting would have left `-100px`.
    ///  - the fourth event keeps the sign: `+200px`, `events = 2`.
    ///  - `+200px` of accumulated travel means UP, so glide = `+50px` and the
    ///    tween is `400px -> 350px` over 140ms. So the assertion below is the
    ///    whole point: netting would have produced `|accumulated| = 0` and *no*
    ///    glide, leaving the position at 400px.
    ///
    /// The **target** lives in position space and is independent of that reset,
    /// because a target is a position, not a direction:
    ///  - `400 + 100 = 500`, then `600`, then `600 - 100 = 500`, then
    ///    `500 - 100 = 400`.
    ///  - the final target is the live position, so `resolve_scroll_tween` finds no
    ///    distance, cancels, and no in-gesture tween survives. The view is
    ///    therefore exactly where it started before the glide, and the glide is
    ///    unambiguously the only motion in the test.
    #[test]
    fn direction_flip_resets_the_wheel_accumulator_instead_of_netting() {
        let viewport = wheel_glide_viewport();
        viewport.sync_from_helix_top_visual_row(20);
        assert_eq!(viewport.scroll_position().y, px(400.0));
        let start = Instant::now();
        for delta in [-100.0, -100.0, 100.0, 100.0] {
            viewport.scroll_by_delta_at(point(px(0.0), px(delta)), start);
        }
        // The four targets net back onto the live position, so nothing is in
        // flight and the position is untouched.
        assert!(
            !viewport.scroll_animation_active(),
            "a target that nets onto the live position left a tween armed"
        );
        assert_eq!(viewport.scroll_position().y, px(400.0));

        let arm = start + IDLE_PROBE;
        assert!(
            viewport.advance_wheel_glide(arm),
            "the post-flip travel never armed a glide, so the accumulator netted"
        );

        assert!(viewport.advance_scroll_animation_at(arm + Duration::from_millis(140)));
        assert_eq!(
            viewport.scroll_position().y,
            px(350.0),
            "the glide followed the direction the gesture ended in, not netted to none"
        );
        assert!(!viewport.scroll_animation_active());
    }

    /// A notch arriving mid-flight retargets from the LIVE position onto the
    /// accumulated destination, and the total is still `N * X`.
    ///
    /// Three ways to get this wrong, and the sample arithmetic tells all three
    /// apart:
    ///
    ///  - **discard the remaining flight** (adding the notch to the live position
    ///    instead of the target): the third notch would ask for `75 + 50 = 125px`,
    ///    so the gesture would land at 125 rather than `3 * 50 = 150px` — less
    ///    than the user asked for,
    ///  - **restart from a stale origin** (re-anchoring onto the position the
    ///    gesture started from rather than the live one): the 20ms sample below
    ///    would read `0 + 150 * quad(0.25) = 65.625px`,
    ///  - **correct**: ease from the live `75px` to the accumulated `150px`, which
    ///    reads `75 + 75 * 0.4375 = 107.8125px` — neither of the other two.
    ///
    /// Arithmetic (three events of `delta.y = -50px`, rows of 20px, no clamp in
    /// range):
    ///  - notch 1 at `start`: target `0 + 50 = 50px`, tween `0 -> 50` over 80ms.
    ///  - notch 2 at `start`: target `50 + 50 = 100px`, tween `0 -> 100` over 80ms.
    ///  - sample at `start + 40ms`: `t = 0.5`, `quad(0.5) = 0.75`, so
    ///    `y = 0 + 100 * 0.75 = 75px`.
    ///  - notch 3 at `start + 40ms`: target `100 + 50 = 150px`, tween `75 -> 150`
    ///    over 80ms from `start + 40ms`, i.e. a deadline of `start + 120ms`.
    ///  - sample at `start + 60ms`: 20ms of that 80ms, `t = 0.25`,
    ///    `quad(0.25) = 0.4375`, so `y = 75 + 75 * 0.4375 = 107.8125px`.
    ///  - sample at `start + 119ms` (79ms of the 80ms) is still in flight; at
    ///    `start + 120ms` it lands on exactly `3 * 50 = 150px`.
    ///
    /// Each notch gets a *fresh* full 80ms, which is what makes a notch feel
    /// like a notch: the deadline is `arming instant + 80ms` every time, not the
    /// original gesture's deadline, which would have expired before the third
    /// notch arrived.
    #[test]
    fn wheel_notch_mid_flight_retargets_from_the_live_position_and_keeps_the_total() {
        let viewport = wheel_glide_viewport();
        let start = Instant::now();
        viewport.scroll_by_delta_at(point(px(0.0), px(-50.0)), start);
        let update = viewport.scroll_by_delta_at(point(px(0.0), px(-50.0)), start);

        // Both notches accumulated onto one target of 2 * 50 = 100px and nothing
        // has moved yet. 100px is row 5 exactly.
        assert_eq!(viewport.scroll_position().y, px(0.0));
        assert_eq!(update.top_visual_row, 5);
        assert_eq!(update.offset_within_row, px(0.0));

        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(40)));
        assert_eq!(viewport.scroll_position().y, px(75.0));

        // The third notch lands mid-flight.
        let update = viewport.scroll_by_delta_at(
            point(px(0.0), px(-50.0)),
            start + Duration::from_millis(40),
        );
        // Reported against the destination, 3 * 50 = 150px = row 7, while the
        // live position is 75px = row 3. 7 - 3 = 4 crossed.
        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 4);
        assert_eq!(update.top_visual_row, 7);
        assert_eq!(update.offset_within_row, px(10.0));
        assert_eq!(viewport.scroll_position().y, px(75.0));

        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(60)));
        assert!(
            (viewport.scroll_position().y - px(107.8125)).abs() < px(0.01),
            "20ms into the retarget read {:?}: 107.8125px eases from the LIVE 75px, \
             65.625px would mean restarting from a stale origin, and 100px would \
             mean discarding the remaining flight",
            viewport.scroll_position().y
        );

        // The retarget's own deadline is `start + 40 + 80 = start + 120ms`, not
        // the first notch's.
        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(119)));
        assert!(
            viewport.scroll_animation_active(),
            "the retargeted tween did not get its own full 80ms"
        );
        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(120)));
        assert_eq!(
            viewport.scroll_position().y,
            px(150.0),
            "three notches of 50px did not land on 3 * 50 = 150px"
        );
        assert!(!viewport.scroll_animation_active());
    }

    /// A new wheel event takes over an in-flight glide from where the view
    /// actually is, not from where the glide was heading.
    ///
    /// The user's wheel is the only live input here, so it has to win
    /// immediately — including the "take over mid-flight" case, where resuming
    /// from the glide's *target* instead of its live position would teleport the
    /// viewport backwards by half the glide.
    ///
    /// The new event starts a *fresh* gesture (`advance_wheel_glide` cleared the
    /// old one), so its target begins from the live position and the tween eases
    /// onto `live + 10px`.
    ///
    /// Arithmetic (reuses the four-event gesture, then a `-10px` event):
    ///  - four notches of 50px accumulate onto 200px; at `start + 40ms` of the
    ///    80ms tween the live position is `0 + 200 * 0.75 = 150px`.
    ///  - the glide arms at `start + IDLE_PROBE` from that live 150px: 200px of
    ///    accumulated wheel travel glides 50px, so the tween is `150px -> 200px`
    ///    over `GLIDE_MS = 140ms`. At `arm + 70ms` the live position is
    ///    `150 + 50 * 0.75 = 187.5px`.
    ///  - the new event cancels the glide and arms its own tween from the live
    ///    `187.5px` to `187.5 + 10 = 197.5px` over 80ms. Resuming from the glide's
    ///    *target* would have asked for `200 + 10 = 210px`, and keeping the glide
    ///    would have asked for `200px`; the landing on exactly `197.5px` rules out
    ///    both.
    ///  - the replacement gesture is a fresh accumulator (1 event, 10px of travel),
    ///    below both gates, so the frame loop stops again once the tween lands.
    #[test]
    fn new_wheel_event_takes_over_an_in_flight_glide_from_its_live_position() {
        let viewport = wheel_glide_viewport();
        let start = Instant::now();
        for _ in 0..4 {
            viewport.scroll_by_delta_at(point(px(0.0), px(-50.0)), start);
        }
        assert!(viewport.advance_scroll_animation_at(start + Duration::from_millis(40)));
        assert_eq!(viewport.scroll_position().y, px(150.0));

        let arm = start + IDLE_PROBE;
        assert!(viewport.advance_wheel_glide(arm));
        assert_eq!(
            viewport.scroll_position().y,
            px(150.0),
            "arming the glide moved the view before the flight"
        );
        assert!(viewport.advance_scroll_animation_at(arm + Duration::from_millis(70)));
        assert_eq!(viewport.scroll_position().y, px(187.5));

        // The flight to 187.5px crossed seven whole rows and legitimately armed
        // the live-position channel. Clearing it isolates the assertion below,
        // which is about what the *wheel event* arms, not about the tween frames.
        assert!(viewport.has_pending_view_sync());
        viewport.clear_pending_view_sync();

        let take = Instant::now();
        let update = viewport.scroll_by_delta_at(point(px(0.0), px(-10.0)), take);

        assert!(update.changed);
        // 197.5px is row 9 with a 17.5px sub-row offset; the live 187.5px is also
        // row 9, so this notch requests no row crossing and arms nothing.
        assert_eq!(update.crossed_visual_rows, 0);
        assert_eq!(update.top_visual_row, 9);
        assert_eq!(update.offset_within_row, px(17.5));
        assert!(
            viewport.scroll_animation_active(),
            "the new wheel event did not take over with its own tween"
        );
        assert!(
            !viewport.has_pending_view_sync(),
            "a tween-armed wheel event armed the live-position Helix channel before \
             the view had crossed a row; that channel belongs to the tween frames"
        );

        assert!(viewport.advance_scroll_animation_at(take + Duration::from_millis(80)));
        assert_eq!(
            viewport.scroll_position().y,
            px(197.5),
            "the new event did not resume from the live 187.5px: 210px would be \
             the glide's target, 200px would be the glide itself"
        );
        assert!(!viewport.scroll_animation_active());

        // The replacement gesture is below both gates, so the loop still ends.
        assert!(viewport.scroll_needs_frames());
        assert!(!viewport.advance_wheel_glide(take + IDLE_PROBE));
        assert!(!viewport.scroll_needs_frames());
    }

    /// A hard fling is capped at `MAX_PX`, in both directions.
    ///
    /// The cap is the only thing standing between a violent wheel and a viewport
    /// that keeps travelling after the user's hand has stopped, so both signs
    /// are pinned. It is applied *after* `RATIO`, which is why the accumulation
    /// has to reach `120 / 0.25 = 480px` before it can saturate.
    ///
    /// Sign convention: negative `delta.y` is DOWN, and a larger position is
    /// further down. So a downward fling eases toward a LARGER position and glides
    /// on to a larger one still.
    ///
    /// Both halves are unchanged by the easing, because the in-gesture tween lands
    /// on exactly the position the 1:1 path would have reached and the glide is
    /// armed from there. Arithmetic (six events of 100px each; the scroll range is
    /// `0..=1200px`, and every target below stays inside it, so nothing is clamped
    /// by the range and the numbers are pure glide arithmetic):
    ///  - down: the targets accumulate `100, 200, ... 600`, so the tween lands on
    ///    `6 * 100 = 600px`; accumulated `-600px`, which clears both gates;
    ///    `clamp(-(-600) * 0.25, ±120) = clamp(150, ±120) = 120px`, so the glide
    ///    tween is `600px -> 720px`. Without the cap it would have been 750px.
    ///  - up: parked at row 50 (`1000px`) rather than near the top, because 600px
    ///    of upward travel has to fit below the position-0 clamp; the targets
    ///    accumulate down to `1000 - 600 = 400px`, accumulated `+600px`,
    ///    `clamp(-(600) * 0.25, ±120) = clamp(-150, ±120) = -120px`, so the tween
    ///    is `400px -> 280px`, still inside the scroll range.
    #[test]
    fn wheel_glide_is_clamped_to_max_px_in_both_directions() {
        let down = wheel_glide_viewport();
        let down_start = Instant::now();
        for _ in 0..6 {
            down.scroll_by_delta_at(point(px(0.0), px(-100.0)), down_start);
        }
        // The eased half lands on the same 600px the 1:1 path used to reach.
        assert!(down.advance_scroll_animation_at(down_start + Duration::from_millis(80)));
        assert_eq!(down.scroll_position().y, px(600.0));
        let down_start = Instant::now();
        let down_arm = down_start + IDLE_PROBE;
        assert!(down.advance_wheel_glide(down_arm));
        assert!(down.advance_scroll_animation_at(down_arm + Duration::from_millis(140)));
        assert_eq!(
            down.scroll_position().y,
            px(720.0),
            "600px of travel glided 120px, the cap, not 150px"
        );

        let up = wheel_glide_viewport();
        up.sync_from_helix_top_visual_row(50);
        assert_eq!(up.scroll_position().y, px(1000.0));
        let up_start = Instant::now();
        for _ in 0..6 {
            up.scroll_by_delta_at(point(px(0.0), px(100.0)), up_start);
        }
        assert!(up.advance_scroll_animation_at(up_start + Duration::from_millis(80)));
        assert_eq!(up.scroll_position().y, px(400.0));
        let up_start = Instant::now();
        let up_arm = up_start + IDLE_PROBE;
        assert!(up.advance_wheel_glide(up_arm));
        assert!(up.advance_scroll_animation_at(up_arm + Duration::from_millis(140)));
        assert_eq!(
            up.scroll_position().y,
            px(280.0),
            "the upward glide was not capped at 120px"
        );
    }

    /// With the feature off, a gesture is exactly today's behaviour: 1:1, instant,
    /// no gesture recorded, no glide, and no frames requested.
    ///
    /// `wheel_glide` is the master switch for the whole wheel model, easing
    /// included, so the tween engine is deliberately left *on* here: if the
    /// in-gesture ease were gated on anything other than this key, the
    /// `smooth_scrolling_enabled()` assertion below would catch it, and if it
    /// were gated on both keys this is the configuration that turns it off.
    ///
    /// Arithmetic (four events of `delta.y = -50px` from `0px`): each event
    /// subtracts, so the positions after each write are `50, 100, 150, 200px` —
    /// `4 * 50 = 200px` exactly, and the same total the enabled fixture reaches
    /// after letting its tweens land. 200px is row 10 with no sub-row offset, and
    /// `200 < 1200` so the range never clamps. The claim is that the flag changes
    /// the timing and the tail and nothing about where the gesture ends up.
    ///
    /// The two clock samples are the load-bearing part. `!scroll_animation_active()`
    /// right after the events already rules out a tween that is armed and waiting,
    /// because an armed tween keeps its state; the far-future sample rules out the
    /// other failure, a tween that *was* armed and then finished without ever being
    /// noticed, which is exactly what a silent stall of the frame loop would look
    /// like from the outside.
    #[test]
    fn disabled_wheel_glide_glides_nothing_and_requests_no_frames() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(400.0), px(800.0)), 100);
        viewport.set_smooth_scrolling(true);
        viewport.set_wheel_glide(false);
        assert!(!viewport.wheel_glide_enabled());
        assert!(
            viewport.smooth_scrolling_enabled(),
            "the tween engine must stay on here, or this test proves nothing about \
             which key gates the easing"
        );

        let start = Instant::now();
        for _ in 0..4 {
            viewport.scroll_by_delta_at(point(px(0.0), px(-50.0)), start);
        }
        assert_eq!(viewport.scroll_position().y, px(200.0));
        assert!(
            !viewport.scroll_animation_active(),
            "a disabled glide still eased the first notch"
        );

        assert!(
            !viewport.scroll_needs_frames(),
            "a disabled glide still started the frame loop"
        );
        assert!(!viewport.advance_scroll_animation_at(start + IDLE_PROBE));
        assert!(!viewport.advance_wheel_glide(start + IDLE_PROBE));
        assert!(!viewport.scroll_needs_frames());
        assert!(!viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position().y, px(200.0));

        // No gesture survived the disabled window, so nothing can fire later, and
        // no tween exists to be found by a clock far past any of their deadlines.
        assert!(!viewport.advance_wheel_glide(start + Duration::from_millis(500)));
        assert!(!viewport.advance_scroll_animation_at(start + Duration::from_secs(5)));
        assert!(!viewport.scroll_needs_frames());
        assert!(!viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position().y, px(200.0));
    }

    /// The axis-only change must not have cost the horizontal sync its actual
    /// job: write x, clamp it into the scrollable width on both sides, and force
    /// x back to zero under soft wrap.
    #[test]
    fn helix_horizontal_sync_sets_and_clamps_x_only() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(400.0), px(800.0)), 100);
        viewport.set_content_width(px(2000.0));
        viewport.set_cell_width(px(10.0));

        assert_eq!(
            viewport.max_scroll_offset().width,
            px(1600.0),
            "2000px of content in a 400px viewport"
        );

        // Park y off the origin so any accidental vertical write is visible.
        viewport.sync_from_helix_top_visual_row(30);
        assert_eq!(viewport.scroll_position(), point(px(0.0), px(600.0)));

        let no_soft_wrap = TextFormat {
            soft_wrap: false,
            viewport_width: 40,
            ..TextFormat::default()
        };
        let soft_wrap = TextFormat {
            soft_wrap: true,
            viewport_width: 40,
            ..TextFormat::default()
        };

        // In range: x follows cell_width * horizontal_offset.
        viewport.sync_from_helix_horizontal_offset(7, &no_soft_wrap);
        assert_eq!(viewport.scroll_position(), point(px(70.0), px(600.0)));

        // Past the end: clamped to max_scroll_offset().width.
        viewport.sync_from_helix_horizontal_offset(10_000, &no_soft_wrap);
        assert_eq!(viewport.scroll_position(), point(px(1600.0), px(600.0)));

        // Soft wrap: the offset is ignored and x is forced to zero.
        viewport.sync_from_helix_horizontal_offset(9, &soft_wrap);
        assert_eq!(viewport.scroll_position(), point(px(0.0), px(600.0)));

        // Back off the soft-wrap zero and confirm y never moved across any of
        // it. The horizontal axis must stay an x-only write.
        viewport.sync_from_helix_horizontal_offset(9, &no_soft_wrap);
        assert_eq!(viewport.scroll_position(), point(px(90.0), px(600.0)));
        viewport.sync_from_helix_horizontal_offset(0, &no_soft_wrap);
        assert_eq!(viewport.scroll_position(), point(px(0.0), px(600.0)));
    }

    /// A wheel notch takes over an in-flight scroll animation, and it takes over
    /// with *its own* tween rather than by dropping the easing.
    ///
    /// The cancel is the original point of the test and is unchanged: a wheel is
    /// a continuous user intent, so it must not fight a discrete jump to
    /// completion. What changed is what replaces the cancelled tween. A notch is
    /// now eased onto over `TWEEN_MS`, so the view must *not* be at the notch's
    /// target the moment the event returns.
    ///
    /// The viewport is 100px tall with 20px rows, so `visible_visual_rows() = 5`
    /// and one page is 5 rows. That matters for showing the old tween is gone:
    ///  - `VisualPages(1)` arms `0px -> 5 * 20 = 100px` over
    ///    `editor_jump_duration(5) = 60 + 6 * 5 = 90ms`. That flight is the one
    ///    the notch has to destroy.
    ///  - The notch is `delta.y = -5px` (down), so the target is
    ///    `0 - (-5) = 5px` and the replacement tween is `0px -> 5px` over
    ///    `TWEEN_MS = 80ms`. Live position is still `0px` on return.
    ///  - At `take + 40ms`, `t = 40 / 80 = 0.5` and `ease_out_quad(0.5) = 0.75`,
    ///    so `y = 0 + 5 * 0.75 = 3.75px`. This single sample is what pins the
    ///    cancel *and* the replacement: the dead 90ms page tween would have been
    ///    at `100 * 0.75 = 75px`, and the old 1:1 instant write would already have
    ///    been at the full `5px`. `3.75px` is neither.
    ///  - `5px` is a fifth of a row, so `crossed_visual_rows` is 0 and the top row
    ///    is still 0 — the notch is sub-row, exactly like the old 1:1 arrival.
    ///  - At `take + 80ms` the tween completes on exactly `5px`.
    ///  - accumulated = `-5px`, so `|accumulated| = 5 < ARM_MIN_PX = 72` and
    ///    `events = 1 < ARM_MIN_EVENTS = 2`: no glide. The frame loop still ends,
    ///    at the gesture clear inside `IDLE_PROBE`.
    #[test]
    fn viewport_wheel_delta_cancels_an_in_flight_scroll_animation() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);
        viewport.set_smooth_scrolling(true);
        assert_eq!(viewport.visible_visual_rows(), 5);
        viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));
        assert!(viewport.scroll_animation_active());
        assert_eq!(
            editor_jump_duration(5),
            Duration::from_millis(90),
            "the cancelled page tween's duration moved; the sample below assumes 90ms"
        );

        let take = Instant::now();
        let update = viewport.scroll_by_delta_at(point(px(0.0), px(-5.0)), take);

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 0);
        assert_eq!(update.top_visual_row, 0);
        assert_eq!(update.offset_within_row, px(5.0));
        assert!(
            viewport.scroll_animation_active(),
            "the wheel notch did not take over with its own tween"
        );
        assert_eq!(
            viewport.scroll_position(),
            point(px(0.0), px(0.0)),
            "the notch jumped straight to its target instead of easing onto it"
        );

        // The 90ms page tween is gone and a 5px/80ms one replaced it.
        assert!(viewport.advance_scroll_animation_at(take + Duration::from_millis(40)));
        assert_eq!(viewport.scroll_position().y, px(3.75));
        assert!(viewport.scroll_animation_active());

        assert!(viewport.advance_scroll_animation_at(take + Duration::from_millis(80)));
        assert_eq!(viewport.scroll_position().y, px(5.0));
        assert!(!viewport.scroll_animation_active());

        // One notch is below both glide gates, so the clear ends the frame loop.
        assert!(viewport.scroll_needs_frames());
        assert!(!viewport.advance_wheel_glide(take + IDLE_PROBE));
        assert!(!viewport.scroll_needs_frames());
    }

    // ---------------------------------------------------------------------
    // Cursor-follow inertia: the keyboard "inertia" added on top of the
    // working cursor-follow ease.
    //
    // One leg. The carry travels further along the direction the gesture was
    // already going, decelerates, and stops — the wheel glide's shape, and for
    // the same reason. There is deliberately no return leg: any reversal in the
    // direction of travel reads as a rebound, and two duration pairs were
    // shipped and measured to find that out (50ms out against 60ms back, then
    // 100ms out against 40ms back). The reasoning is recorded where the carry is
    // armed, in `arm_scrolloff_overshoot`.
    //
    // Every expected number below is derived from the *intended* behaviour and
    // the constants, with the arithmetic shown, rather than read out of the
    // implementation. Two earlier rounds of work in this file derived their
    // expectations from the code's own formula, and thereby pinned an inverted
    // glide that shipped green through a full suite; the direction assertions in
    // particular are written against the *cursor's offset within the viewport*,
    // which is the property that actually matters and which an implementation
    // that inverts its sign cannot fake.
    // ---------------------------------------------------------------------

    /// How far past the reveal clock a test probes the idle-window decision.
    /// Well clear of `GESTURE_IDLE_MS = 120ms` and of
    /// `editor_jump_duration(1) = 66ms`, and it is always read *after* the
    /// reveal that stamped the gesture, so the window it is compared against is
    /// already behind it. Same discipline as the wheel's `IDLE_PROBE`.
    const KEY_IDLE_PROBE: Duration = Duration::from_millis(200);

    /// The cursor row the first `j` in a gesture starts from on the fixture:
    /// `20 + (40 - 5) = 55`, the band's firing edge. A press at cursor `C`
    /// targets `C + 5 + 1 - 40 = C - 34`, so press `n` is `C = 54 + n` and
    /// lands on `20 + n`.
    const FIRST_FORWARD_CURSOR: usize = 55;
    /// The cursor row the first `k` in a gesture starts from: `20 + 5 - 1 = 24`,
    /// the last row *inside* the lower edge. The backward band fires only when
    /// `C < top + 5`, so press `n` is `C = FIRST_BACKWARD_CURSOR + 1 - n` —
    /// 24, 23, 22, 21 — and targets `C - 5 = 20 - n`.
    const FIRST_BACKWARD_CURSOR: usize = 24;

    /// The cursor-follow inertia fixture: `scrolloff_reveal_viewport(200)` parked
    /// at row 20 through the scrollbar.
    ///
    /// The scrollbar is the cancel-on-purpose path, so it leaves no tween in
    /// flight and is a clean idle origin — which matters because every test here
    /// starts a gesture from scratch and a stray tween would be sampled as part
    /// of it.
    ///
    /// Row 20 rather than row 0, so that neither direction of the first reveal
    /// can saturate against a document edge: a carry that clamps takes a
    /// different path through the arming (it refuses to arm), and these tests
    /// are about the arithmetic of an unclamped one. The scroll range is
    /// `20 * 200 - 800 = 3200px`, i.e. row 160, which every target quoted below
    /// stays well inside.
    fn key_inertia_viewport() -> EditorViewport {
        let viewport = scrolloff_reveal_viewport(200);
        viewport.scroll_to_vertical_position_from_scrollbar(px(400.0));
        assert_eq!(viewport.top_visual_row(), 20, "fixture setup");
        assert!(
            !viewport.scroll_animation_active(),
            "fixture setup: the scrollbar is the cancel-on-purpose path"
        );
        viewport
    }

    /// Press `j` `count` times, one `Scrolloff` reveal per press.
    ///
    /// The band is measured from the tween's *destination*, so consecutive
    /// presses each produce a one-row travel even though the view has not moved
    /// between them — the same property the existing `Scrolloff` tests rely on.
    /// Each press arms a fresh full-duration tween from the live position, so
    /// only the last one's target is left to land.
    ///
    /// The target row is asserted here rather than derived in each test, so that
    /// a change to the band shows up as a fixture failure instead of as a
    /// silently different carry. The tween is deliberately *not* asserted here:
    /// this helper is also used with the engine off, where the same rows are
    /// reached by instant writes and nothing may arm.
    fn press_j_toward_the_bottom_margin(viewport: &EditorViewport, count: usize) {
        for n in 1..=count {
            let cursor = FIRST_FORWARD_CURSOR + n - 1;
            let update = viewport.reveal_visual_row(cursor, EditorCursorReveal::Scrolloff, 5);
            assert_eq!(
                update.top_visual_row,
                20 + n,
                "press {n} at cursor {cursor} chose the wrong target"
            );
        }
    }

    /// Press `k` `count` times. The exact mirror of
    /// [`press_j_toward_the_bottom_margin`]: the cursor walks *up* the same
    /// number of rows, so the gesture's accumulated travel is `-count`.
    fn press_k_toward_the_top_margin(viewport: &EditorViewport, count: usize) {
        for n in 1..=count {
            let cursor = FIRST_BACKWARD_CURSOR + 1 - n;
            let update = viewport.reveal_visual_row(cursor, EditorCursorReveal::Scrolloff, 5);
            assert_eq!(
                update.top_visual_row,
                20usize.saturating_sub(n),
                "press {n} at cursor {cursor} chose the wrong target"
            );
        }
    }

    /// Let a reveal chain's last tween land, and hand back the instant from which
    /// the idle window can be probed.
    ///
    /// `editor_jump_duration(1) = 60 + 6 = 66ms`, so sampling half a second out
    /// is exactly `t = 1` however long the press loop itself took. The returned
    /// instant is read *after* the landing, so `+ KEY_IDLE_PROBE` from it is
    /// unambiguously past the `GESTURE_IDLE_MS = 120ms` window measured from the
    /// real clock that stamped the last reveal.
    fn land_reveal_tweens(viewport: &EditorViewport) -> Instant {
        assert!(
            viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500)),
            "the reveal chain did not land"
        );
        assert!(
            !viewport.scroll_animation_active(),
            "the reveal chain landed but left a tween behind"
        );
        Instant::now() + KEY_IDLE_PROBE
    }

    /// Press `j` `count` times and land the chain, returning the instant from
    /// which the idle window can be probed.
    fn j_gesture(viewport: &EditorViewport, count: usize) -> Instant {
        press_j_toward_the_bottom_margin(viewport, count);
        land_reveal_tweens(viewport)
    }

    // ---------------------------------------------------------------------
    // Where to sample the carry, derived rather than hardcoded.
    //
    // These are the sample *times* only. The carry's duration is the feel, and
    // the feel is meant to be tuned: a suite that hardcoded `50ms` and `60ms`
    // pinned the numbers instead of the mechanism, so a single tune invalidated
    // five tests that were never about the tune. The mechanism is the exact
    // landing — the carry ends on its clamped target, to the pixel — and that
    // stays asserted.
    //
    // So every offset below is a fraction of the duration the implementation
    // actually uses, and every expected position in every test is derived from
    // the same fraction via `ease_out_quad`. The doc comment on each test spells
    // the arithmetic out with the fractions, so a tune moves the sample and its
    // expected position together and the test still means the same thing.
    // ---------------------------------------------------------------------

    /// A quarter of the way through the carry, so `t = 0.25`.
    fn outward_quarter() -> Duration {
        OVERSHOOT_DURATION / 4
    }

    /// One millisecond before the carry ends, so `t = 0.99`.
    ///
    /// The sample exists to show the carry had *not* finished, and
    /// `ease_out_quad(0.99) = 2 * 0.99 - 0.99^2 = 0.9999`, so the view is
    /// `1 - 0.9999 = 0.01%` of the carry short of its target — on a 20px carry
    /// that is two thousandths of a pixel, close enough to look landed and
    /// provably not. Derived as an offset rather than written as a literal so it
    /// tracks the duration.
    fn outward_just_before_landing() -> Duration {
        OVERSHOOT_DURATION - Duration::from_millis(1)
    }

    /// The instant the carry lands exactly, `t = 1`.
    fn outward_landing() -> Duration {
        OVERSHOOT_DURATION
    }

    /// Comfortably past the end of the carry, for the "nothing is waiting to
    /// fire later" probes.
    ///
    /// Measured from the carry's own landing rather than written as a flat 500ms,
    /// so a future tune that made the carry *longer* could not quietly push the
    /// probe back inside it — which would turn "nothing re-arms" into "the probe
    /// was too early to see anything".
    fn long_after_the_carry() -> Duration {
        outward_landing() + Duration::from_millis(500)
    }

    /// Press `j` at `cursor` and report the row the viewport will be on once the
    /// reveal lands.
    ///
    /// [`press_j_toward_the_bottom_margin`] cannot serve the steady-state test
    /// below: it asserts the `20 + n` progression, which only holds from a cold
    /// fixture, whereas from the second round on the band origin is wherever the
    /// last carry stopped. It reports rather than asserts so each test can state
    /// the row it expects, and it says nothing about whether a reveal fired at
    /// all — for a cursor still inside the band the reported row is just the row
    /// the view is already on, which is the case the steady-state test needs to
    /// pin.
    fn press_j_at(viewport: &EditorViewport, cursor: usize) -> usize {
        viewport
            .reveal_visual_row(cursor, EditorCursorReveal::Scrolloff, 5)
            .top_visual_row
    }

    /// Repeated carries hold the margin still instead of ratcheting it outward.
    ///
    /// This is the guard for the property the feature is *built* on, and it is
    /// the one that was originally got wrong: a settle leg was added to stop the
    /// margin creeping, on the belief that the creep was unbounded. It is not.
    /// The carry is measured from the *fresh* margin row of the gesture's last
    /// reveal, and that row comes from the cursor and the current band origin, so
    /// every round re-anchors and the previous carry is already inside the base
    /// rather than added to it. Worked through on this fixture, with
    /// `visible_rows = 40` and `scrolloff = 5`:
    ///
    ///  - the band fires forward at `cursor >= top + 40 - 5 = top + 35`, and the
    ///    target row is `cursor + 5 + 1 - 40 = cursor - 34`,
    ///  - so after a reveal `cursor - top = 34`, i.e. the cursor sits exactly
    ///    `margin = 5` rows above the last visible row,
    ///  - after a carry of `c` rows it sits `margin + c` rows above it, and
    ///    `c = min(rows * 0.25, OVERSHOOT_MAX = 2)`, so the distance can never
    ///    exceed `5 + 2 = 7` and is exactly 7 for any gesture that reaches the
    ///    cap at `2 / 0.25 = 8` rows.
    ///
    /// Three rounds of a capped gesture, and the distance is 7 every time:
    ///
    /// ```text
    /// round 1  idle presses 53, 54 (inside the band: no reveal, no gesture)
    ///          8 presses, cursors 55..62, rows 20 -> 28
    ///          carry 2 rows: 28 -> 30, cursor 62, distance (30 + 39) - 62 = 7
    /// round 2  idle presses 63, 64 (the cursor walks back in from 7 rows inside)
    ///          8 presses, cursors 65..72, rows 28 -> 38
    ///          carry 2 rows: 38 -> 40, cursor 72, distance (40 + 39) - 72 = 7
    /// round 3  idle presses 73, 74
    ///          8 presses, cursors 75..82, rows 40 -> 48
    ///          carry 2 rows: 48 -> 50, cursor 82, distance (50 + 39) - 82 = 7
    /// ```
    ///
    /// The idle presses are part of the mechanism rather than padding: after a
    /// carry the cursor is `OVERSHOOT_MAX` rows further inside the band than the
    /// firing edge, so it has to walk back in before the band fires again. A
    /// press that does not fire the band is not a step, so it neither joins the
    /// gesture nor refreshes its idle window — and the eight presses of a round
    /// therefore all arrive inside `GESTURE_IDLE_MS` of each other and form one
    /// gesture of exactly 8 rows, which is what puts the carry on its cap.
    ///
    /// **If the margin did ratchet**, the distance would grow by a full
    /// `OVERSHOOT_MAX` every round — 7, 9, 11 — because that is what "the carry
    /// is applied on top of where the last one stopped" means. Round 2 would then
    /// fail as `9 != 7` and round 3 as `11 != 7`, and after `n` rounds the
    /// configured `scrolloff = 5` would be silently `5 + 2n` rows, which at
    /// `visible_rows = 40` puts the cursor off the top of the viewport after 17
    /// rounds. That is the failure this test exists to make impossible.
    ///
    /// Upward is the same arithmetic mirrored — the backward target is
    /// `cursor - margin`, so the rows above the cursor after a carry are
    /// `margin + c` — and
    /// [`backward_gesture_carries_upward_and_adds_margin_symmetrically`] covers
    /// one cycle of it.
    #[test]
    fn repeated_carries_hold_the_margin_instead_of_ratcheting() {
        let viewport = key_inertia_viewport();
        let visible_rows = viewport.visible_visual_rows();
        assert_eq!(visible_rows, 40, "test fixture drifted: 40 visible rows");
        let distance_from_bottom_edge = |viewport: &EditorViewport, cursor: usize| -> usize {
            (viewport.top_visual_row() + visible_rows - 1) - cursor
        };

        // `(first cursor of the round's eight presses, row the round starts on,
        // row its last reveal lands on, row the carry ends on)`.
        let rounds = [(55, 20, 28, 30), (65, 30, 38, 40), (75, 40, 48, 50)];

        for (round, &(first_cursor, start_row, margin_row, carried_row)) in
            rounds.iter().enumerate()
        {
            // Two cursor moves that are still inside the band: no reveal, so no
            // gesture, and the view must not move.
            for cursor in first_cursor - 2..first_cursor {
                assert_eq!(
                    press_j_at(&viewport, cursor),
                    start_row,
                    "round {round}: cursor {cursor} is inside the band, so the press \
                     must not move the view"
                );
            }

            // Eight one-row reveals: `2 / OVERSHOOT_RATIO` rows of gesture, which
            // is exactly enough to take the carry to its cap.
            for step in 0..8usize {
                let cursor = first_cursor + step;
                assert_eq!(
                    press_j_at(&viewport, cursor),
                    margin_row - 7 + step,
                    "round {round}: press {step} of eight, at cursor {cursor}"
                );
            }

            let arm = land_reveal_tweens(&viewport);
            assert_eq!(
                viewport.scroll_position().y,
                px((margin_row * 20) as f32),
                "round {round}: the round's last reveal landed on row {margin_row}"
            );

            assert!(
                viewport.advance_scrolloff_inertia(arm),
                "round {round}: an eight-row gesture armed no carry"
            );
            assert!(viewport.advance_scroll_animation_at(arm + outward_landing()));
            assert_eq!(
                viewport.top_visual_row(),
                carried_row,
                "round {round}: the carry is two rows past row {margin_row}"
            );
            assert_eq!(
                distance_from_bottom_edge(&viewport, first_cursor + 7),
                7,
                "round {round}: the cursor must end `scrolloff + OVERSHOOT_MAX` rows \
                 from the bottom edge every time, not further in than the round before"
            );

            // The carry landing is the last transition in the feature: one leg,
            // and then `Idle`.
            assert!(
                !viewport.advance_scrolloff_inertia(arm + outward_landing()),
                "round {round}: a landed carry armed a second leg"
            );
            assert!(
                !viewport.scroll_needs_frames(),
                "round {round}: a completed carry left the frame loop running"
            );
        }
    }

    /// One deliberate `j` is a precision movement, and it has to stay one.
    ///
    /// A single press is one reveal and one row of travel, which is what an
    /// ordinary cursor-follow reveal always is: the band fires the moment the
    /// cursor crosses the margin and the target restores it, so press `n`
    /// travels exactly `+1` row. The gesture therefore accumulates `1` row, and
    /// `MIN_GESTURE_ROWS = 3` is not reached — no carry is ever armed, and the view
    /// eases to `21 * 20 = 420px` and stops there.
    ///
    /// The minimum is a row count precisely so that this holds on any window
    /// size or line height: one row can never reach three rows, so a single press
    /// can never carry no matter how the constants are tuned.
    ///
    /// The last two assertions are the load-bearing ones. The gesture has to be
    /// *dropped*, not merely declined, and it has to stay dropped — otherwise the
    /// render pass would request frames forever after one keypress.
    #[test]
    fn single_scrolloff_reveal_carries_nothing_and_stops_the_frame_loop() {
        let viewport = key_inertia_viewport();
        let arm = j_gesture(&viewport, 1);

        assert_eq!(viewport.top_visual_row(), 21, "the reveal landed on its target");
        assert_eq!(viewport.scroll_position(), point(px(0.0), px(420.0)));

        assert!(
            !viewport.advance_scrolloff_inertia(arm),
            "one row of travel armed a carry"
        );
        assert_eq!(
            viewport.scroll_position(),
            point(px(0.0), px(420.0)),
            "a declined gesture moved the view"
        );
        assert!(
            !viewport.scroll_needs_frames(),
            "a declined gesture left the frame loop running"
        );

        // And nothing is waiting to fire later either.
        assert!(!viewport.advance_scrolloff_inertia(arm + long_after_the_carry()));
        assert!(!viewport.scroll_needs_frames());
        assert_eq!(viewport.scroll_position(), point(px(0.0), px(420.0)));
    }

    /// A four-press gesture carries one row further and stops there.
    ///
    /// Carry arithmetic, from the constants and nothing else:
    ///  - four presses at one row each is `rows = 4`, and `4 >= MIN_GESTURE_ROWS = 3`,
    ///  - `OVERSHOOT_RATIO = 0.25` gives `4 * 0.25 = 1.0` row, and `1.0 <= OVERSHOOT_MAX = 2`
    ///    so the cap does not bind,
    ///  - `1.0` rows at a 20px line height is `20px`,
    ///  - the gesture travelled **down**, so the carry is `+20px` past the
    ///    margin: `24 * 20 = 480px -> 500px`, rows 24 -> 25, and that is where
    ///    the view stays.
    ///
    /// Every offset below is a fraction of the duration the implementation
    /// actually uses — see the sample-offset helpers just above — so retuning the
    /// feel moves the samples and the arithmetic with it instead of breaking
    /// this test. The arithmetic is written in fractions of the duration, not in
    /// milliseconds, for the same reason.
    ///
    /// The carry runs `480px -> 500px` over `OVERSHOOT_DURATION`:
    ///  - a quarter of the way in, `t = 0.25`, and
    ///    `ease_out_quad(0.25) = 2 * 0.25 - 0.25^2 = 0.5 - 0.0625 = 0.4375`, so
    ///    `y = 480 + 20 * 0.4375 = 480 + 8.75 = 488.75px`,
    ///  - `0.99` of the way in, `ease_out_quad(0.99) = 2 * 0.99 - 0.99^2 =
    ///    1.98 - 0.9801 = 0.9999`, so `y = 480 + 20 * 0.9999 = 499.998px`: two
    ///    thousandths of a pixel short of the target, and still in flight, which
    ///    is what makes the duration an assertion rather than an assumption,
    ///  - at the end, `t = 1` exactly, so `y = 500px` and it deactivates.
    ///
    /// There is no second leg, and that is the assertion rather than an
    /// omission: a return would move the position *back* towards `480px`, and
    /// **any reversal in the direction of travel reads as a rebound** no matter
    /// which of the two lasts longer. Two pairs were shipped and measured — 50ms
    /// out against 60ms back, then 100ms out against 40ms back — and both read
    /// as one. A reversal is noticed for existing, not for being fast, so the
    /// duration was never the variable. The carry is the wheel glide's shape: out,
    /// decelerating, done.
    #[test]
    fn multi_reveal_gesture_carries_further_and_stops_there() {
        let viewport = key_inertia_viewport();
        let arm = j_gesture(&viewport, 4);
        assert_eq!(
            viewport.scroll_position().y,
            px(480.0),
            "four one-row presses: 24 * 20px"
        );

        assert!(
            viewport.advance_scrolloff_inertia(arm),
            "a four-row gesture armed no carry"
        );
        assert!(
            viewport.scroll_needs_frames(),
            "the carry has to keep the frame loop alive while it flies"
        );
        assert!(viewport.advance_scroll_animation_at(arm + outward_quarter()));
        assert_eq!(
            viewport.scroll_position().y,
            px(488.75),
            "480 + 20 * 0.4375, a quarter of the way through the carry"
        );
        assert!(viewport.advance_scroll_animation_at(arm + outward_just_before_landing()));
        assert!(
            viewport.scroll_animation_active(),
            "the carry finished before OVERSHOOT_DURATION had elapsed"
        );
        assert!(
            viewport.scroll_position().y < px(500.0),
            "the carry reached its target before OVERSHOOT_DURATION had elapsed"
        );
        assert!(viewport.advance_scroll_animation_at(arm + outward_landing()));
        assert_eq!(
            viewport.scroll_position().y,
            px(500.0),
            "the carry did not land on 480 + 4 * 0.25 rows"
        );
        assert_eq!(viewport.top_visual_row(), 25, "one row further down the document");
        assert!(!viewport.scroll_animation_active());

        // The landing is the last transition in the feature: there is nothing
        // left to arm, so the state goes to `Idle` and the frame loop with it.
        assert!(
            !viewport.advance_scrolloff_inertia(arm + outward_landing()),
            "the carry landing armed a second leg"
        );
        assert!(!viewport.scroll_needs_frames(), "a landed carry left the frame loop running");
        assert!(!viewport.advance_scrolloff_inertia(arm + long_after_the_carry()));
        assert!(!viewport.scroll_needs_frames());
        assert_eq!(
            viewport.scroll_position().y,
            px(500.0),
            "the view drifted after the carry landed"
        );
    }

    /// A two-row gesture does not carry: `MIN_GESTURE_ROWS - 1 = 2`.
    ///
    /// The minimum is a row count, so the way to sit exactly on the wrong side of
    /// it is to press one time fewer than a qualifying gesture: two presses,
    /// `2 * 1 = 2` rows, `2 < 3`, no carry. The view is left on `20 + 2 = 22`,
    /// i.e. `22 * 20 = 440px`, which is the row the second reveal asked for and
    /// the row the `scrolloff` margin puts the cursor at.
    #[test]
    fn below_threshold_gesture_carries_nothing() {
        let viewport = key_inertia_viewport();
        let arm = j_gesture(&viewport, 2);
        assert_eq!(viewport.scroll_position().y, px(440.0), "22 * 20px");

        assert!(
            !viewport.advance_scrolloff_inertia(arm),
            "two rows of travel armed a carry"
        );
        assert_eq!(
            viewport.scroll_position().y,
            px(440.0),
            "a declined gesture moved the view"
        );
        assert!(
            !viewport.scroll_needs_frames(),
            "a below-threshold gesture left the frame loop running"
        );
        assert!(!viewport.advance_scrolloff_inertia(arm + long_after_the_carry()));
        assert!(!viewport.scroll_needs_frames());
        assert_eq!(viewport.scroll_position().y, px(440.0));
    }

    /// A long hold carries `OVERSHOOT_MAX` rows and no more.
    ///
    /// Twelve presses is `12 * 1 = 12` rows, and the cap is applied *after* the
    /// ratio: `12 * OVERSHOOT_RATIO = 12 * 0.25 = 3.0` rows, held down to
    /// `OVERSHOOT_MAX = 2.0` rows. At a 20px line height that is `40px`, so the
    /// carry runs `32 * 20 = 640px -> 680px` — rows 32 to 34 — and stops. Uncapped
    /// it would have been `60px` and row 35, which is what makes the cap
    /// load-bearing rather than decorative: this test fails if the `min` is ever
    /// moved below the ratio, or dropped.
    ///
    /// The cap is also the bound on how far the carry can ever move the cursor
    /// *inside* the band, and therefore on how far the `scrolloff` margin can be
    /// pushed outward; that argument is spelled out where the carry is armed, and
    /// pinned across repeated rounds by
    /// [`repeated_carries_hold_the_margin_instead_of_ratcheting`].
    ///
    /// The fixture's scroll range is `20 * 200 - 800 = 3200px` (row 160), so
    /// nothing here is clipped by the end of the document and the numbers are
    /// pure carry arithmetic.
    #[test]
    fn scrolloff_carry_is_capped_at_the_maximum_rows() {
        let viewport = key_inertia_viewport();
        let arm = j_gesture(&viewport, 12);
        assert_eq!(viewport.scroll_position().y, px(640.0), "32 * 20px");

        assert!(viewport.advance_scrolloff_inertia(arm));
        assert!(viewport.advance_scroll_animation_at(arm + outward_landing()));
        assert_eq!(
            viewport.scroll_position().y,
            px(680.0),
            "12 * 0.25 = 3 rows should have been held down to OVERSHOOT_MAX = 2 rows = 40px"
        );
        assert_eq!(viewport.top_visual_row(), 34, "640 + 40 = 680px, which is row 34");

        assert!(!viewport.advance_scrolloff_inertia(arm + outward_landing()));
        assert!(!viewport.scroll_needs_frames());
    }

    /// The carry runs *along* the travel direction, and that is the whole reason
    /// it is safe.
    ///
    /// The sign of the position is not the claim. The claim is about the
    /// **cursor**: a downward gesture comes to rest with the cursor against the
    /// `scrolloff = 5` margin above the last visible row, so scrolling further
    /// down has to move the cursor's offset within the viewport *down*, further
    /// from that edge. A carry that pushed the cursor the other way would pass
    /// every distance check, and would only be visible to a user holding `j`.
    ///
    /// After four presses from row 20 the cursor is at row `54 + 4 = 58`, and
    /// `visible_visual_rows() = 40` means the viewport shows rows `top ..= top+39`:
    ///  - at rest the view is on row 24, so the offset is `58 - 24 = 34` and the
    ///    margin below is `(24 + 39) - 58 = 5` — exactly `scrolloff`,
    ///  - a quarter of the way through the carry the position is
    ///    `480 + 20 * 0.4375 = 488.75px`, still row `floor(488.75/20) = 24`, so
    ///    the offset is still 34,
    ///  - at the end (`500px`, row 25) the offset is `58 - 25 = 33`, one row
    ///    *more* of margin, not one row less: 6 rows below the cursor, and the
    ///    view stops there.
    #[test]
    fn downward_gesture_carries_downward_and_adds_margin() {
        let viewport = key_inertia_viewport();
        let arm = j_gesture(&viewport, 4);
        let cursor_row = 58;
        let visible_rows = viewport.visible_visual_rows();
        assert_eq!(visible_rows, 40, "test fixture drifted: 40 visible rows");

        let offset_in_viewport = |viewport: &EditorViewport| -> isize {
            cursor_row as isize - viewport.top_visual_row() as isize
        };
        let margin_below = |viewport: &EditorViewport| -> usize {
            (viewport.top_visual_row() + visible_rows - 1) - cursor_row
        };

        assert_eq!(offset_in_viewport(&viewport), 34);
        assert_eq!(margin_below(&viewport), 5, "the scrolloff margin, at rest");

        assert!(viewport.advance_scrolloff_inertia(arm));
        assert!(viewport.advance_scroll_animation_at(arm + outward_quarter()));
        assert_eq!(
            viewport.scroll_position().y,
            px(488.75),
            "480 + 20 * 0.4375, a quarter of the way through the carry"
        );
        assert_eq!(viewport.top_visual_row(), 24, "floor(488.75 / 20) = 24");

        assert!(viewport.advance_scroll_animation_at(arm + outward_landing()));
        assert_eq!(
            viewport.scroll_position().y,
            px(500.0),
            "a downward carry must increase the position, because a larger position is further down"
        );
        assert_eq!(viewport.top_visual_row(), 25);
        assert_eq!(
            offset_in_viewport(&viewport),
            33,
            "the carry must push the cursor up into the viewport, never down toward the bottom edge"
        );
        assert_eq!(margin_below(&viewport), 6, "the carry added a row of margin");
        assert!(
            (0..visible_rows as isize).contains(&offset_in_viewport(&viewport)),
            "the cursor left the painted band during the carry"
        );

        // The landing ends the feature, so the driver arms nothing further.
        assert!(!viewport.advance_scrolloff_inertia(arm + outward_landing()));
        assert!(!viewport.scroll_needs_frames());
        assert_eq!(
            offset_in_viewport(&viewport),
            33,
            "the cursor must still be inside the viewport once the carry has landed"
        );
        assert_eq!(margin_below(&viewport), 6, "the added margin is where the view rests");
    }

    /// `k` is the mirror image of `j`, in the position *and* in the safety
    /// argument.
    ///
    /// Four presses from row 20 put the cursor at row `25 - 4 = 21` with the view
    /// on row `20 - 4 = 16`, an offset of 5 rows from the top — exactly the
    /// `scrolloff = 5` margin above the top edge. The gesture's travel is `-4`
    /// rows, so `4 * 0.25 = 1.0` row = 20px, and because the gesture travelled
    /// **up** the carry is `16 * 20 = 320px - 20px = 300px`: the position
    /// *decreases*, rows 16 -> 15, and stops there.
    ///
    /// The safety claim is the mirror of the downward one: a smaller position
    /// means the cursor's offset within the viewport *increases*, away from the
    /// top edge. So the carry again only ever adds margin — 5 rows above the
    /// cursor become 6.
    #[test]
    fn backward_gesture_carries_upward_and_adds_margin_symmetrically() {
        let viewport = key_inertia_viewport();
        press_k_toward_the_top_margin(&viewport, 4);
        let arm = land_reveal_tweens(&viewport);
        let cursor_row = 21;
        let visible_rows = viewport.visible_visual_rows();
        assert_eq!(visible_rows, 40, "test fixture drifted: 40 visible rows");

        // At the *top* margin the rows above the cursor and the cursor's offset
        // within the viewport are the same number, so `offset_in_viewport`
        // measures both at once and carries the safety claim on its own. (In the
        // downward test they are different quantities, hence two closures there.)
        let offset_in_viewport = |viewport: &EditorViewport| -> isize {
            cursor_row as isize - viewport.top_visual_row() as isize
        };

        assert_eq!(viewport.scroll_position().y, px(320.0), "16 * 20px");
        assert_eq!(offset_in_viewport(&viewport), 5, "the scrolloff margin, at rest");

        assert!(viewport.advance_scrolloff_inertia(arm));
        assert!(viewport.advance_scroll_animation_at(arm + outward_quarter()));
        assert_eq!(
            viewport.scroll_position().y,
            px(311.25),
            "320 - 20 * 0.4375, a quarter of the way through the carry"
        );
        assert!(
            viewport.scroll_animation_active(),
            "the carry finished before OVERSHOOT_DURATION had elapsed"
        );

        assert!(viewport.advance_scroll_animation_at(arm + outward_landing()));
        assert_eq!(
            viewport.scroll_position().y,
            px(300.0),
            "an upward carry must decrease the position, because a smaller position is further up"
        );
        assert_eq!(viewport.top_visual_row(), 15);
        assert_eq!(
            offset_in_viewport(&viewport),
            6,
            "the carry must push the cursor down into the viewport, never up past the top edge"
        );
        assert!((0..visible_rows as isize).contains(&offset_in_viewport(&viewport)));

        // The landing ends the feature, so the driver arms nothing further.
        assert!(!viewport.advance_scrolloff_inertia(arm + outward_landing()));
        assert!(!viewport.scroll_needs_frames());
        assert_eq!(
            offset_in_viewport(&viewport),
            6,
            "the cursor must still be inside the viewport once the carry has landed"
        );
    }

    /// A direction reversal restarts the accumulation instead of netting against
    /// it, exactly as `record_wheel_gesture` does.
    ///
    /// The numbers are chosen so that the two readings disagree. Four `j` presses
    /// are `+4` rows, and the upward reveal below travels `-4`, so a net sum
    /// would be `0` and produce no carry at all, while a reset restarts at `-4`
    /// and carries *upwards*. The peak position is therefore the discriminating
    /// assertion.
    ///
    /// That upward reveal is a four-row jump rather than the one-row jump a `k`
    /// hold makes, and it has to be: from row 24 the backward band's lower edge
    /// is `24 + 5 = 29`, so a one-row upward press would have to put the cursor
    /// below 29 to reveal at all, and it does not. A cursor at 25 is four rows
    /// inside the margin — where an intervening search or mode change lands — and
    /// it reveals with `target = 25 - 5 = 20`, i.e. `travel = 20 - 24 = -4`, well
    /// inside the `visible_rows - margin = 35` snap threshold.
    ///
    /// The carry is `4 * 0.25 = 1.0` row = 20px **upwards** from the
    /// `20 * 20 = 400px` margin: `400px -> 380px`, rows 20 -> 19, and it stops
    /// there. A net would have left the view on `400px` throughout.
    #[test]
    fn direction_reversal_resets_the_gesture_rather_than_netting_it() {
        let viewport = key_inertia_viewport();
        j_gesture(&viewport, 4);
        assert_eq!(
            viewport.scroll_position().y,
            px(480.0),
            "four j presses: row 24"
        );

        // One upward reveal of -4 rows, arriving while the +4-row gesture is
        // still collecting. The four presses are *landed* first, so the live row
        // is the band origin (24) and the new reveal has a real 4-row distance to
        // travel. Left in flight they would share a live row, the new target
        // would land on the position already on screen, and the reveal would be
        // correctly refused as a zero-distance step instead of a reversal.
        let update = viewport.reveal_visual_row(25, EditorCursorReveal::Scrolloff, 5);
        assert_eq!(update.top_visual_row, 20, "25 - 5");
        let arm = land_reveal_tweens(&viewport);
        assert_eq!(viewport.scroll_position().y, px(400.0), "row 20");

        assert!(
            viewport.advance_scrolloff_inertia(arm),
            "the gesture netted to +4 - 4 = 0 rows, which carries nothing"
        );
        assert!(viewport.advance_scroll_animation_at(arm + outward_landing()));
        assert_eq!(
            viewport.scroll_position().y,
            px(380.0),
            "the carry must be upwards: the reversal restarted the accumulator at -4 rows"
        );
        assert_eq!(viewport.top_visual_row(), 19);

        // The landing ends the feature, so the driver arms nothing further and
        // the view stays where the carry put it.
        assert!(!viewport.advance_scrolloff_inertia(arm + outward_landing()));
        assert!(!viewport.scroll_needs_frames());
        assert_eq!(
            viewport.scroll_position().y,
            px(380.0),
            "the view drifted after the carry landed"
        );
    }

    /// Every route out of the feature has to leave the frame loop off.
    ///
    /// Three cases, and the middle one is the one this feature could most
    /// plausibly ship broken: a gesture that clears its idle window but fails
    /// its threshold. If that clear were conditional, `scroll_needs_frames()`
    /// would stay true and the editor would request frames forever after two `j`
    /// presses. The first case also asserts the positive half of the argument —
    /// the loop has to stay *on* while the gesture is still collecting, or the
    /// window could never elapse and the carry would never be decided at all —
    /// and the two steps after the carry's landing are the whole termination
    /// argument for the `Outward` phase: the tween is spent, so the driver
    /// clears the phase and the loop is off on the very next frame.
    #[test]
    fn scrolloff_inertia_always_leaves_the_frame_loop_off() {
        // A qualifying gesture, run all the way through its one carry.
        let carried = key_inertia_viewport();
        let arm = j_gesture(&carried, 4);
        assert!(carried.advance_scrolloff_inertia(arm));
        assert!(
            carried.scroll_needs_frames(),
            "a carry in flight has to keep the frame loop alive"
        );
        assert!(carried.advance_scroll_animation_at(arm + outward_landing()));
        assert!(!carried.advance_scrolloff_inertia(arm + outward_landing()));
        assert!(!carried.scroll_needs_frames(), "a completed carry left the frame loop running");
        // And it has to stay off: nothing is waiting to re-arm.
        assert!(!carried.advance_scrolloff_inertia(arm + long_after_the_carry()));
        assert!(!carried.scroll_needs_frames());

        // A below-threshold gesture: the same decision point, the other branch.
        let declined = key_inertia_viewport();
        press_j_toward_the_bottom_margin(&declined, 2);
        let arm = land_reveal_tweens(&declined);
        assert!(
            declined.scroll_needs_frames(),
            "a collecting gesture has to keep the frame loop alive until its window elapses"
        );
        assert!(!declined.advance_scrolloff_inertia(arm));
        assert!(!declined.scroll_needs_frames(), "a declined gesture left the frame loop running");

        // The engine off: the feature must not even ask for a frame.
        let disabled = scrolloff_reveal_viewport(200);
        disabled.set_smooth_scrolling(false);
        assert!(!disabled.smooth_scrolling_enabled());
        press_j_toward_the_bottom_margin(&disabled, 4);
        assert_eq!(disabled.scroll_position().y, px(480.0), "row 24, reached instantly");
        assert!(
            !disabled.scroll_needs_frames(),
            "a disabled feature asked the render pass for a frame"
        );
        assert!(!disabled.advance_scrolloff_inertia(Instant::now() + KEY_IDLE_PROBE));
        assert!(!disabled.scroll_needs_frames());
    }

    /// With smooth scrolling off the feature is a complete no-op.
    ///
    /// The same four presses are made, but every reveal snaps instead of
    /// easing, so none of them is a glide and none of them may feed a gesture.
    /// The view still lands on row 24 — `480px`, reached by four instant writes
    /// rather than by one tween — because the band is a property of the cursor
    /// and the margins and is unchanged by the switch. What must not happen is
    /// anything after that: no carry armed, no extra frame requested, and probing
    /// far past the idle window changes nothing at all.
    #[test]
    fn scrolloff_inertia_is_inert_when_smooth_scrolling_is_off() {
        let viewport = scrolloff_reveal_viewport(200);
        viewport.scroll_to_vertical_position_from_scrollbar(px(400.0));
        viewport.set_smooth_scrolling(false);
        assert!(!viewport.smooth_scrolling_enabled());
        assert_eq!(viewport.top_visual_row(), 20, "fixture setup");

        for n in 1..=4 {
            let update = viewport.reveal_visual_row(54 + n, EditorCursorReveal::Scrolloff, 5);
            assert_eq!(
                update.top_visual_row,
                20 + n,
                "the band is unchanged by the switch; only the route to the row is"
            );
            assert!(
                !viewport.scroll_animation_active(),
                "press {n} armed a tween with the engine off"
            );
            assert!(
                !viewport.scroll_needs_frames(),
                "press {n} made a disabled feature ask for a frame"
            );
        }
        assert_eq!(viewport.scroll_position().y, px(480.0), "row 24, reached instantly");

        let probe = Instant::now() + KEY_IDLE_PROBE;
        assert!(!viewport.advance_scrolloff_inertia(probe));
        assert_eq!(
            viewport.scroll_position().y,
            px(480.0),
            "a disabled feature moved the view"
        );
        assert!(!viewport.advance_scrolloff_inertia(probe + Duration::from_secs(5)));
        assert!(!viewport.scroll_needs_frames());
        assert_eq!(viewport.scroll_position().y, px(480.0));
    }

    #[test]
    fn viewport_cursor_reveal_request_cancels_an_in_flight_scroll_animation() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);
        viewport.set_smooth_scrolling(true);
        viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));
        assert!(viewport.scroll_animation_active());

        viewport.apply_scroll_request(EditorViewportScrollRequest::CursorReveal(
            EditorCursorReveal::Center,
        ));

        assert!(!viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position().y, px(0.0));
        assert_eq!(
            viewport.take_cursor_reveal_request(),
            Some(EditorCursorReveal::Center)
        );
    }

    #[test]
    fn viewport_scrollbar_drag_cancels_an_in_flight_scroll_animation() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);
        viewport.set_smooth_scrolling(true);
        viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));
        assert!(viewport.scroll_animation_active());

        viewport.scroll_to_vertical_position_from_scrollbar(px(20.0));

        assert!(!viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position().y, px(20.0));
    }

    /// The three *deliberate alignment* reveals are instant, and stay instant
    /// with smooth scrolling on.
    ///
    /// `align_view_top`/`_center`/`_bottom` are user-issued jumps, not cursor
    /// follow, so a tween there would put latency between the keypress and the
    /// requested alignment. Only `Scrolloff` eases. `scrolloff` is 0 throughout
    /// so the margin cannot muddy the arithmetic, and the numbers below are
    /// read straight off the alignment rule, not off a helper.
    ///
    /// Arithmetic, with 800x100 bounds and a 20px line height:
    /// `visible_visual_rows() = floor(100 / 20) = 5`.
    ///  - `Top`: target is the cursor row itself -> `20` -> `20 * 20 = 400px`.
    ///  - `Center`: `visual_row - visible_rows / 2 = 20 - 2 = 18` -> `360px`.
    ///  - `Bottom`: `visual_row - (visible_rows - 1) = 20 - 4 = 16` -> `320px`.
    #[test]
    fn viewport_explicit_alignment_reveals_stay_instant_with_smooth_scrolling() {
        let cases = [
            (EditorCursorReveal::Top, px(400.0), 20),
            (EditorCursorReveal::Center, px(360.0), 18),
            (EditorCursorReveal::Bottom, px(320.0), 16),
        ];

        for (reveal, expected_y, expected_row) in cases {
            let mut viewport = EditorViewport::new(px(20.0));
            viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 100);
            viewport.set_smooth_scrolling(true);

            let update = viewport.reveal_visual_row(20, reveal, 0);

            assert_eq!(viewport.visible_visual_rows(), 5, "test fixture drifted");
            assert!(update.changed, "{reveal:?} did not move the view");
            assert!(
                !viewport.scroll_animation_active(),
                "{reveal:?} armed a tween; only Scrolloff may ease"
            );
            assert_eq!(viewport.scroll_position().y, expected_y, "{reveal:?}");
            assert_eq!(viewport.top_visual_row(), expected_row, "{reveal:?}");
        }
    }

    #[test]
    fn viewport_page_cursor_sync_direction_excludes_cursor_page_requests() {
        assert_eq!(
            EditorViewportScrollRequest::VisualPageFraction {
                pages: 1,
                divisor: 2,
            }
            .page_cursor_sync_direction(),
            Some(EditorViewportScrollDirection::Forward)
        );
        assert_eq!(
            EditorViewportScrollRequest::VisualPageWithCursor {
                pages: 1,
                divisor: 2,
            }
            .page_cursor_sync_direction(),
            None
        );
    }

    #[test]
    fn viewport_cursor_request_resolves_top_center_and_bottom_rows() {
        assert_eq!(
            EditorViewportCursorRequest {
                target: EditorViewportCursorTarget::Top,
                count: 1,
            }
            .target_visual_row(10, 20, 100, 5),
            15
        );
        assert_eq!(
            EditorViewportCursorRequest {
                target: EditorViewportCursorTarget::Top,
                count: 3,
            }
            .target_visual_row(10, 20, 100, 5),
            17
        );
        assert_eq!(
            EditorViewportCursorRequest {
                target: EditorViewportCursorTarget::Center,
                count: 1,
            }
            .target_visual_row(10, 20, 100, 5),
            19
        );
        assert_eq!(
            EditorViewportCursorRequest {
                target: EditorViewportCursorTarget::Bottom,
                count: 1,
            }
            .target_visual_row(10, 20, 100, 5),
            24
        );
        assert_eq!(
            EditorViewportCursorRequest {
                target: EditorViewportCursorTarget::Bottom,
                count: 3,
            }
            .target_visual_row(10, 20, 100, 5),
            22
        );
    }

    #[test]
    fn viewport_scroll_request_can_defer_cursor_reveal() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);

        let update = viewport.apply_scroll_request(EditorViewportScrollRequest::CursorReveal(
            EditorCursorReveal::Center,
        ));

        assert!(!update.changed);
        assert_eq!(viewport.scroll_position().y, px(0.0));
        assert_eq!(update.top_visual_row, 0);
        assert!(!viewport.has_pending_view_sync());
        assert_eq!(
            viewport.take_cursor_reveal_request(),
            Some(EditorCursorReveal::Center)
        );
    }

    #[test]
    fn viewport_reports_scrollbar_position_changes() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(400.0)), 100);

        let update = viewport.scroll_to_vertical_position_from_scrollbar(px(65.0));

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 3);
        assert_eq!(update.top_visual_row, 3);
        assert_eq!(update.offset_within_row, px(5.0));
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_clamps_scrollbar_position() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 10);

        let update = viewport.scroll_to_vertical_position_from_scrollbar(px(500.0));

        assert!(update.changed);
        assert_eq!(viewport.scroll_position().y, px(100.0));
        assert_eq!(update.top_visual_row, 5);
    }

    #[test]
    fn viewport_cursor_reveal_requests_are_shared_across_clones() {
        let viewport = EditorViewport::new(px(20.0));
        let clone = viewport.clone();

        viewport.request_cursor_reveal(EditorCursorReveal::Scrolloff);

        assert_eq!(
            clone.pending_cursor_reveal_request(),
            Some(EditorCursorReveal::Scrolloff)
        );
        assert_eq!(
            clone.take_cursor_reveal_request(),
            Some(EditorCursorReveal::Scrolloff)
        );
        assert_eq!(viewport.pending_cursor_reveal_request(), None);
    }

    #[test]
    fn viewport_cursor_reveal_requests_use_latest_request() {
        let viewport = EditorViewport::new(px(20.0));

        viewport.request_cursor_reveal(EditorCursorReveal::Scrolloff);
        viewport.request_cursor_reveal(EditorCursorReveal::Center);

        assert_eq!(
            viewport.take_cursor_reveal_request(),
            Some(EditorCursorReveal::Center)
        );
        assert_eq!(viewport.take_cursor_reveal_request(), None);
    }

    #[test]
    fn viewport_cursor_reveal_keeps_visible_rows_unchanged() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 100);
        viewport.sync_from_helix_top_visual_row(10);

        let update = viewport.ensure_visual_row_visible(12, 1);

        assert!(!update.changed);
        assert_eq!(update.crossed_visual_rows, 0);
        assert_eq!(viewport.top_visual_row(), 10);
        assert!(!viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_cursor_reveal_scrolls_above_margin() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 100);
        viewport.sync_from_helix_top_visual_row(10);

        let update = viewport.ensure_visual_row_visible(7, 2);

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, -5);
        assert_eq!(viewport.top_visual_row(), 5);
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_cursor_reveal_scrolls_below_margin() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 100);
        viewport.sync_from_helix_top_visual_row(10);

        let update = viewport.ensure_visual_row_visible(14, 1);

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 1);
        assert_eq!(viewport.top_visual_row(), 11);
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_cursor_reveal_clamps_scrolloff_for_small_viewports() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(40.0)), 100);
        viewport.sync_from_helix_top_visual_row(10);

        let update = viewport.ensure_visual_row_visible(12, 10);

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 1);
        assert_eq!(viewport.top_visual_row(), 11);
    }

    #[test]
    fn viewport_cursor_reveal_can_center_visual_row() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 100);
        viewport.sync_from_helix_top_visual_row(0);

        let update = viewport.reveal_visual_row(20, EditorCursorReveal::Center, 0);

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 18);
        assert_eq!(viewport.top_visual_row(), 18);
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_cursor_reveal_can_align_visual_row_top() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 100);
        viewport.sync_from_helix_top_visual_row(0);

        let update = viewport.reveal_visual_row(20, EditorCursorReveal::Top, 0);

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 20);
        assert_eq!(viewport.top_visual_row(), 20);
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_cursor_reveal_can_align_visual_row_bottom() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 100);
        viewport.sync_from_helix_top_visual_row(0);

        let update = viewport.reveal_visual_row(20, EditorCursorReveal::Bottom, 0);

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 16);
        assert_eq!(viewport.top_visual_row(), 16);
        assert!(viewport.has_pending_view_sync());
    }

    #[test]
    fn viewport_reveal_scrolls_to_max_when_bottom_row_is_partial() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(99.0)), 10);

        assert_eq!(viewport.visible_visual_rows(), 4);

        let update = viewport.reveal_visual_row(9, EditorCursorReveal::Scrolloff, 0);

        assert!(update.changed);
        assert_eq!(viewport.top_visual_row(), 5);
        assert_eq!(viewport.offset_within_row(), px(1.0));
        assert_eq!(
            viewport.scroll_position().y,
            viewport.max_scroll_offset().height
        );
    }

    #[test]
    fn viewport_preserves_fractional_offset_for_same_helix_row() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(400.0)), 100);
        viewport.scroll_by_delta(point(px(0.0), px(-25.0)));

        viewport.sync_from_helix_top_visual_row(1);

        assert_eq!(viewport.top_visual_row(), 1);
        assert_eq!(viewport.offset_within_row(), px(5.0));
    }

    #[test]
    fn viewport_uses_visual_row_count_for_scroll_range() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 30);

        assert_eq!(viewport.content_visual_rows(), 30);
        assert_eq!(viewport.max_scroll_offset().height, px(500.0));
    }

    #[test]
    fn content_layout_sync_updates_visual_row_count_from_document_metrics() {
        let (document, view) = test_document_and_view("one\ntwo\nthree\n");
        let mut viewport = EditorViewport::new(px(20.0));
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(80.0)));
        viewport.set_viewport_size(bounds.size);

        let update = viewport.sync_content_layout(
            &document,
            &view,
            EditorViewportContentLayout {
                theme: None,
                bounds,
                cell_width: px(8.0),
                minimum_columns: 1,
                extra_gutter_columns: 0,
            },
        );

        let expected = EditorDocumentMetrics::resolve_for_view(
            &document,
            &view,
            None,
            bounds,
            update.gutter_columns,
            px(8.0),
            1,
        );

        assert_eq!(update.visual_rows, expected.visual_rows);
        assert_eq!(update.soft_wrap, expected.soft_wrap);
        assert_eq!(viewport.content_visual_rows(), expected.visual_rows);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn surface_layout_sync_returns_none_for_missing_view() {
        let (mut editor, doc_id, _view_id) = test_editor_with_text("one\ntwo\nthree\n");
        let mut viewport = EditorViewport::new(px(20.0));
        let layout = EditorViewportSurfaceLayout {
            theme: None,
            bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(80.0))),
            cell_width: px(8.0),
            line_height: px(20.0),
            minimum_columns: 1,
            extra_gutter_columns: 0,
            scrolloff: Config::default().scrolloff,
            cursor_reveal: None,
        };

        assert!(
            viewport
                .sync_surface_layout(&mut editor, doc_id, ViewId::default(), layout)
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn surface_layout_sync_returns_none_for_missing_document() {
        let (mut editor, _doc_id, view_id) = test_editor_with_text("one\ntwo\nthree\n");
        let missing_doc_id = editor.new_file(Action::Load);
        assert!(editor.close_document(missing_doc_id, true).is_ok());
        let mut viewport = EditorViewport::new(px(20.0));
        let layout = EditorViewportSurfaceLayout {
            theme: None,
            bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(80.0))),
            cell_width: px(8.0),
            line_height: px(20.0),
            minimum_columns: 1,
            extra_gutter_columns: 0,
            scrolloff: Config::default().scrolloff,
            cursor_reveal: None,
        };

        assert!(editor.tree.try_get(view_id).is_some());
        assert!(editor.document(missing_doc_id).is_none());
        assert!(
            viewport
                .sync_surface_layout(&mut editor, missing_doc_id, view_id, layout)
                .is_none()
        );
    }

    #[test]
    fn editor_layout_constructors_use_shared_minimum_columns() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(80.0)));

        let content_layout = EditorViewportContentLayout::for_editor(None, bounds, px(8.0));
        let surface_layout =
            EditorViewportSurfaceLayout::for_editor(None, bounds, px(8.0), px(20.0), 0, None);

        assert_eq!(
            content_layout.minimum_columns,
            EDITOR_MINIMUM_VIEWPORT_COLUMNS
        );
        assert_eq!(
            surface_layout.minimum_columns,
            EDITOR_MINIMUM_VIEWPORT_COLUMNS
        );
    }

    #[test]
    fn helix_view_area_includes_gutter_and_status_row() {
        assert_eq!(
            helix_view_area_for_surface(4, 20, 5),
            Rect::new(0, 0, 24, 6)
        );
    }

    #[test]
    fn helix_view_area_plan_reports_target_and_change() {
        let previous_area = Rect::new(0, 0, 10, 4);

        let plan = helix_view_area_plan_for_surface(previous_area, 4, 20, 5);

        assert!(plan.changed);
        assert_eq!(plan.previous_area, previous_area);
        assert_eq!(plan.target_area, Rect::new(0, 0, 24, 6));
    }

    #[test]
    fn helix_view_area_plan_reports_noop_for_matching_area() {
        let previous_area = Rect::new(0, 0, 24, 6);

        let plan = helix_view_area_plan_for_surface(previous_area, 4, 20, 5);

        assert!(!plan.changed);
        assert_eq!(plan.previous_area, previous_area);
        assert_eq!(plan.target_area, previous_area);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn surface_layout_sync_uses_native_cells_without_rewriting_tree_area() {
        let (mut editor, doc_id, view_id) = test_editor_with_text("one\ntwo\nthree\n");
        let mut viewport = EditorViewport::new(px(20.0));
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(101.0)));
        let original_area = editor.tree.get(view_id).area;

        let update = viewport
            .sync_surface_layout(
                &mut editor,
                doc_id,
                view_id,
                EditorViewportSurfaceLayout {
                    theme: None,
                    bounds,
                    cell_width: px(8.0),
                    line_height: px(20.0),
                    minimum_columns: 1,
                    extra_gutter_columns: 0,
                    scrolloff: Config::default().scrolloff,
                    cursor_reveal: None,
                },
            )
            .unwrap();

        let document = editor.document(doc_id).unwrap();
        let view = editor.tree.get(view_id);
        let expected = EditorDocumentMetrics::resolve_for_view(
            document,
            view,
            None,
            bounds,
            update.gutter_columns,
            px(8.0),
            1,
        );

        assert_eq!(view.area, original_area);
        assert_eq!(
            update.view_area_plan.target_area,
            helix_view_area_for_surface(
                update.gutter_columns,
                expected.viewport_columns,
                viewport.visible_visual_rows()
            )
        );
        assert!(update.view_area_plan.changed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn surface_layout_sync_preserves_split_tree_area() {
        let (mut editor, doc_id, _view_id) = test_editor_with_text("one\ntwo\nthree\n");
        editor.switch(doc_id, Action::VerticalSplit);

        let split_view_id = editor.tree.focus;
        let original_area = editor.tree.get(split_view_id).area;
        assert!(original_area.x > 0);

        let mut viewport = EditorViewport::new(px(20.0));
        let update = viewport
            .sync_surface_layout(
                &mut editor,
                doc_id,
                split_view_id,
                EditorViewportSurfaceLayout {
                    theme: None,
                    bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(101.0))),
                    cell_width: px(8.0),
                    line_height: px(20.0),
                    minimum_columns: 1,
                    extra_gutter_columns: 0,
                    scrolloff: Config::default().scrolloff,
                    cursor_reveal: None,
                },
            )
            .unwrap();

        assert_eq!(editor.tree.get(split_view_id).area, original_area);
        assert_eq!(update.view_area_plan.target_area.x, 0);
        assert_ne!(update.view_area_plan.target_area, original_area);
    }

    #[test]
    fn surface_layout_sync_for_view_updates_native_and_helix_layout() {
        let (mut document, mut view) = test_document_and_view("one\ntwo\nthree\n");
        let view_id = view.id;
        let mut viewport = EditorViewport::new(px(20.0));
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(101.0)));

        let update = viewport.sync_surface_layout_for_view(
            &mut document,
            &mut view,
            view_id,
            EditorViewportSurfaceLayout {
                theme: None,
                bounds,
                cell_width: px(8.0),
                line_height: px(20.0),
                minimum_columns: 1,
                extra_gutter_columns: 0,
                scrolloff: Config::default().scrolloff,
                cursor_reveal: None,
            },
        );

        assert_eq!(view.area, update.view_area_plan.target_area);
        assert_eq!(update.gutter_columns, view.gutter_offset(&document));
        assert_eq!(viewport.content_visual_rows(), update.visual_rows);
    }

    #[test]
    fn surface_layout_sync_reserves_extra_gutter_columns() {
        let (mut document, mut view) = test_document_and_view("one\ntwo\nthree\n");
        let view_id = view.id;
        let mut viewport = EditorViewport::new(px(20.0));
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(101.0)));
        let extra_gutter_columns = 3;

        let update = viewport.sync_surface_layout_for_view(
            &mut document,
            &mut view,
            view_id,
            EditorViewportSurfaceLayout {
                theme: None,
                bounds,
                cell_width: px(8.0),
                line_height: px(20.0),
                minimum_columns: 1,
                extra_gutter_columns,
                scrolloff: Config::default().scrolloff,
                cursor_reveal: None,
            },
        );

        assert_eq!(
            update.gutter_columns,
            view.gutter_offset(&document)
                .saturating_add(extra_gutter_columns)
        );
        assert_eq!(
            viewport.viewport_bounds().size,
            editor_text_viewport_size_for_bounds(bounds, update.gutter_columns, px(8.0))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cursor_reveal_keeps_native_scroll_authoritative_when_helix_offset_is_stale() {
        let text = (0..50)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let (mut editor, doc_id, view_id) = test_editor_with_text(&text);
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(240.0), px(100.0)), 50);
        viewport.sync_from_helix_top_visual_row(10);

        {
            let doc = editor.document_mut(doc_id).unwrap();
            let cursor = doc.text().line_to_char(12);
            doc.set_selection(view_id, Selection::point(cursor));
            let stale_anchor = doc.text().line_to_char(20);
            doc.set_view_offset(
                view_id,
                ViewPosition {
                    anchor: stale_anchor,
                    vertical_offset: 0,
                    horizontal_offset: 0,
                },
            );
        }

        let update = viewport
            .sync_surface_layout(
                &mut editor,
                doc_id,
                view_id,
                EditorViewportSurfaceLayout {
                    theme: None,
                    bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(101.0))),
                    cell_width: px(8.0),
                    line_height: px(20.0),
                    minimum_columns: 1,
                    extra_gutter_columns: 0,
                    scrolloff: Config::default().scrolloff,
                    cursor_reveal: Some(EditorCursorReveal::Scrolloff),
                },
            )
            .unwrap();

        assert!(!update.cursor_revealed);
        assert!(update.helix_view_synced);
        assert_eq!(viewport.top_visual_row(), 10);
        assert_eq!(update.helix_snapshot.top_visual_row, 10);

        let doc = editor.document(doc_id).unwrap();
        assert_eq!(doc.text().char_to_line(doc.view_offset(view_id).anchor), 10);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn surface_layout_roundtrips_scroll_to_trailing_newline_eof() {
        let (mut editor, doc_id, view_id) = test_editor_with_text("one\ntwo\n");
        let mut viewport = EditorViewport::new(px(20.0));
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(21.0)));
        let layout = EditorViewportSurfaceLayout {
            theme: None,
            bounds,
            cell_width: px(8.0),
            line_height: px(20.0),
            minimum_columns: 1,
            extra_gutter_columns: 0,
            scrolloff: Config::default().scrolloff,
            cursor_reveal: None,
        };

        viewport
            .sync_surface_layout(&mut editor, doc_id, view_id, layout)
            .unwrap();
        let eof_row = viewport.content_visual_rows().saturating_sub(1);
        viewport.scroll_to_vertical_position_from_scrollbar(viewport.max_scroll_offset().height);

        let update = viewport
            .sync_surface_layout(&mut editor, doc_id, view_id, layout)
            .unwrap();

        assert!(update.helix_view_synced);
        assert_eq!(viewport.top_visual_row(), eof_row);
        assert_eq!(update.helix_snapshot.top_visual_row, eof_row);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn surface_layout_resize_clamps_bottom_scroll_and_resyncs_helix() {
        let text = (0..30)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let (mut editor, doc_id, view_id) = test_editor_with_text(&text);
        let mut viewport = EditorViewport::new(px(20.0));
        let narrow_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(101.0)));
        let tall_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(201.0)));
        let narrow_layout = EditorViewportSurfaceLayout {
            theme: None,
            bounds: narrow_bounds,
            cell_width: px(8.0),
            line_height: px(20.0),
            minimum_columns: 1,
            extra_gutter_columns: 0,
            scrolloff: Config::default().scrolloff,
            cursor_reveal: None,
        };
        let tall_layout = EditorViewportSurfaceLayout {
            bounds: tall_bounds,
            ..narrow_layout
        };

        viewport
            .sync_surface_layout(&mut editor, doc_id, view_id, narrow_layout)
            .unwrap();
        viewport.scroll_to_vertical_position_from_scrollbar(viewport.max_scroll_offset().height);
        viewport
            .sync_surface_layout(&mut editor, doc_id, view_id, narrow_layout)
            .unwrap();

        let update = viewport
            .sync_surface_layout(&mut editor, doc_id, view_id, tall_layout)
            .unwrap();
        let expected_top_visual_row = viewport.top_visual_row();

        assert!(update.helix_view_synced);
        assert_eq!(
            viewport.scroll_position().y,
            viewport.max_scroll_offset().height
        );
        assert_eq!(
            update.helix_snapshot.top_visual_row,
            expected_top_visual_row
        );

        let doc = editor.document(doc_id).unwrap();
        assert_eq!(
            doc.text().char_to_line(doc.view_offset(view_id).anchor),
            expected_top_visual_row
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cursor_reveal_uses_trailing_newline_eof_visual_row() {
        let (mut editor, doc_id, view_id) = test_editor_with_text("one\ntwo\n");
        let mut viewport = EditorViewport::new(px(20.0));
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(21.0)));
        {
            let doc = editor.document_mut(doc_id).unwrap();
            doc.set_selection(view_id, Selection::point(doc.text().len_chars()));
        }

        let update = viewport
            .sync_surface_layout(
                &mut editor,
                doc_id,
                view_id,
                EditorViewportSurfaceLayout {
                    theme: None,
                    bounds,
                    cell_width: px(8.0),
                    line_height: px(20.0),
                    minimum_columns: 1,
                    extra_gutter_columns: 0,
                    scrolloff: Config::default().scrolloff,
                    cursor_reveal: Some(EditorCursorReveal::Scrolloff),
                },
            )
            .unwrap();

        let eof_row = viewport.content_visual_rows().saturating_sub(1);
        assert!(update.cursor_revealed);
        assert!(update.helix_view_synced);
        assert_eq!(viewport.top_visual_row(), eof_row);
        assert_eq!(update.helix_snapshot.top_visual_row, eof_row);
    }

    /// A tween armed *during* paint is the one thing the pre-paint frame driver
    /// cannot see, so the paint has to hand the frame loop a follow-up.
    ///
    /// `view_component.rs` consults `scroll_needs_frames()` at the top of
    /// `render`, which has already run by the time `sync_surface_layout` applies
    /// the cursor reveal. Before the follow-up frame existed, a `Scrolloff` reveal
    /// that eased was armed with nothing left to drive it and the view sat still
    /// for good: the tween was live, the loop had ended, and the user saw a
    /// frozen viewport.
    ///
    /// This reproduces that ordering exactly — the driver's own check runs first
    /// and declines to ask for a frame, then the paint arms the tween, then a
    /// driver pass runs to completion. The loop mirrors the one in
    /// `view_component.rs`, with an injected clock so the samples are exact
    /// arithmetic and the test is neither slow nor timing-sensitive.
    ///
    /// Fixture: 801px-tall bounds make the viewport `(801 - 1) = 800px`, so
    /// `visible_visual_rows() = floor(800 / 20) = 40`. The document is 200 lines
    /// of `line N`, which is far wider than the ~26 text columns a 240px surface
    /// leaves after the gutter, so nothing soft-wraps and the cursor's visual row
    /// is its line number. `scrolloff = 5` with the cursor parked on line 35:
    /// `upper = 0 + (40 - 5) = 35`, so `35 >= 35` fires, the target is
    /// `35 + 5 + 1 - 40 = 1`, and `travel_rows = 1` gives a 66ms eased reveal
    /// rather than an instant jump.
    #[tokio::test(flavor = "current_thread")]
    async fn paint_that_arms_a_scrolloff_tween_needs_the_follow_up_frame_to_run_it() {
        let text = (0..200)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let (mut editor, doc_id, view_id) = test_editor_with_text(&text);
        {
            let doc = editor.document_mut(doc_id).unwrap();
            let cursor = doc.text().line_to_char(35);
            doc.set_selection(view_id, Selection::point(cursor));
        }
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_smooth_scrolling(true);
        let layout = EditorViewportSurfaceLayout {
            theme: None,
            bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(801.0))),
            cell_width: px(8.0),
            line_height: px(20.0),
            minimum_columns: 1,
            extra_gutter_columns: 0,
            scrolloff: 5,
            cursor_reveal: Some(EditorCursorReveal::Scrolloff),
        };

        // The driver's check, run where `render` runs it: before the paint below,
        // nothing wants a frame, so no follow-up would be scheduled.
        assert!(
            !viewport.scroll_needs_frames(),
            "the driver would have asked for a frame before anything moved"
        );
        assert!(!viewport.scroll_animation_active());

        let update = viewport
            .sync_surface_layout(&mut editor, doc_id, view_id, layout)
            .unwrap();

        assert_eq!(
            viewport.visible_visual_rows(),
            40,
            "test fixture drifted: the target row below assumes 40 visible rows"
        );
        assert!(
            update.cursor_revealed,
            "the paint armed motion without reporting it, so the render pass cannot \
             schedule the follow-up frame that drives it"
        );
        assert!(
            viewport.scroll_animation_active(),
            "Scrolloff armed no tween, so this test would pass without the follow-up frame"
        );
        assert_eq!(
            viewport.top_visual_row(),
            0,
            "the tween is armed but has not moved the viewport yet"
        );
        // The view-position plan is built from the *live* row, so it is still 0
        // here: Helix is synced to what is on screen, and the tween's own
        // whole-row crossing is what arms the sync that follows the ease. The
        // tween surviving this paint is the assertion that matters, and it is the
        // one the view-position sync above could have broken.
        assert_eq!(update.view_position_plan.top_visual_row, 0);

        // The driver's pass, 16ms at a time.
        //
        // This loop is a miniature of the real driver in `view_component.rs` and
        // must call the same advances in the same order. `scroll_needs_frames()`
        // has three terms — the in-flight tween, a pending wheel gesture, and a
        // pending cursor-follow inertia — and only the matching driver can clear
        // the latter two. A loop that advanced the tween alone spins forever the
        // moment a reveal leaves an inertia collecting, and the failure reads as
        // "the feature never terminates" rather than "this copy drifted". When a
        // term is added to that predicate, its driver belongs here too.
        //
        // `frames` is a bound, not an expectation: the loop's clock starts after
        // the tween's own, so the count depends on how long the arming call took.
        // What must hold is that the loop terminates and lands the reveal.
        let mut now = Instant::now();
        let mut frames = 0;
        while viewport.scroll_needs_frames() {
            now += Duration::from_millis(16);
            viewport.advance_scroll_animation_at(now);
            viewport.advance_wheel_glide(now);
            viewport.advance_scrolloff_inertia(now);
            frames += 1;
            assert!(frames <= 16, "the frame loop did not terminate");
        }
        assert!(frames > 0, "the loop never ran, so the tween was never driven");

        assert_eq!(
            viewport.scroll_position().y,
            px(20.0),
            "1 row over editor_jump_duration(1) = 66ms"
        );
        assert_eq!(viewport.top_visual_row(), 1, "the reveal never landed on its target");
        assert!(
            viewport.has_pending_view_sync(),
            "crossing a whole row must arm the Helix sync"
        );
    }

    #[test]
    fn surface_viewport_size_uses_text_area_height() {
        let bounds = Bounds::new(point(px(10.0), px(20.0)), size(px(300.0), px(101.0)));

        assert_eq!(
            editor_viewport_size_for_bounds(bounds),
            size(px(300.0), px(100.0))
        );
    }

    #[test]
    fn surface_viewport_size_clamps_empty_height() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(300.0), px(0.5)));

        assert_eq!(
            editor_viewport_size_for_bounds(bounds),
            size(px(300.0), px(0.0))
        );
    }

    #[test]
    fn helix_viewport_snapshot_reports_top_visual_row() {
        let text = "one\ntwo\nthree";
        let text_format = TextFormat::default();
        let annotations = default_annotations();
        let snapshot = helix_viewport_snapshot(
            text.into(),
            ViewPosition {
                anchor: 4,
                vertical_offset: 2,
                horizontal_offset: 7,
            },
            &text_format,
            &annotations,
        );

        assert_eq!(
            snapshot,
            HelixViewportSnapshot {
                anchor_line: 1,
                vertical_offset: 2,
                top_visual_row: 3,
            }
        );
    }

    #[test]
    fn helix_viewport_snapshot_clamps_stale_anchor() {
        let text = "one\ntwo";
        let text_format = TextFormat::default();
        let annotations = default_annotations();
        let snapshot = helix_viewport_snapshot(
            text.into(),
            ViewPosition {
                anchor: 1_000,
                vertical_offset: 0,
                horizontal_offset: 0,
            },
            &text_format,
            &annotations,
        );

        assert_eq!(snapshot.anchor_line, 1);
        assert_eq!(snapshot.top_visual_row, 1);
    }

    #[test]
    fn helix_viewport_snapshot_uses_soft_wrap_visual_rows() {
        let text = "abcdef\nzz";
        let text_format = TextFormat {
            soft_wrap: true,
            viewport_width: 3,
            ..TextFormat::default()
        };
        let annotations = default_annotations();
        let snapshot = helix_viewport_snapshot(
            text.into(),
            ViewPosition {
                anchor: 7,
                vertical_offset: 0,
                horizontal_offset: 0,
            },
            &text_format,
            &annotations,
        );

        assert_eq!(snapshot.anchor_line, 1);
        assert!(snapshot.top_visual_row > snapshot.anchor_line);
    }

    #[test]
    fn helix_viewport_snapshot_adds_vertical_offset_to_visual_row() {
        let text = "abcdef\nzz";
        let text_format = TextFormat {
            soft_wrap: true,
            viewport_width: 3,
            ..TextFormat::default()
        };
        let annotations = default_annotations();
        let snapshot = helix_viewport_snapshot(
            text.into(),
            ViewPosition {
                anchor: 7,
                vertical_offset: 2,
                horizontal_offset: 0,
            },
            &text_format,
            &annotations,
        );

        assert_eq!(snapshot.anchor_line, 1);
        assert!(snapshot.top_visual_row >= snapshot.anchor_line + 2);
    }

    #[test]
    fn conversion_cache_reuses_matching_view_position_plan() {
        let base = EditorViewportConversionBaseKey {
            document_version: 1,
            text_len: 12,
            view_id: ViewId::default(),
            text_format: EditorViewportTextFormatKey::from(&TextFormat::default()),
        };
        let key = EditorViewportViewPositionPlanCacheKey {
            base: base.clone(),
            previous_view_position: ViewPosition::default(),
            top_visual_row: 3,
            horizontal_offset: 0,
        };
        let plan = EditorViewportViewPositionPlan {
            top_visual_row: 3,
            previous_view_position: ViewPosition::default(),
            view_position: ViewPosition {
                anchor: 7,
                vertical_offset: 0,
                horizontal_offset: 0,
            },
            changed: true,
        };
        let mut cache = EditorViewportConversionCache::default();

        assert_eq!(cache.view_position_plan(&key), None);
        cache.store_view_position_plan(key.clone(), plan);
        assert_eq!(cache.view_position_plan(&key), Some(plan));

        let changed_key = EditorViewportViewPositionPlanCacheKey {
            top_visual_row: 4,
            ..key
        };
        assert_eq!(cache.view_position_plan(&changed_key), None);
        assert_eq!(
            cache.stats(),
            EditorViewportConversionStats {
                view_position_plan_hits: 1,
                view_position_plan_misses: 2,
                ..EditorViewportConversionStats::default()
            }
        );
    }

    #[test]
    fn conversion_cache_reuses_matching_cursor_visual_row() {
        let base = EditorViewportConversionBaseKey {
            document_version: 1,
            text_len: 12,
            view_id: ViewId::default(),
            text_format: EditorViewportTextFormatKey::from(&TextFormat::default()),
        };
        let key = DocumentCursorVisualRowCacheKey {
            base: base.clone(),
            cursor_char_idx: 8,
        };
        let mut cache = EditorViewportConversionCache::default();

        assert_eq!(cache.cursor_visual_row(&key), None);
        cache.store_cursor_visual_row(key.clone(), 4);
        assert_eq!(cache.cursor_visual_row(&key), Some(4));

        let changed_key = DocumentCursorVisualRowCacheKey {
            cursor_char_idx: 9,
            ..key
        };
        assert_eq!(cache.cursor_visual_row(&changed_key), None);
        assert_eq!(
            cache.stats(),
            EditorViewportConversionStats {
                cursor_visual_row_hits: 1,
                cursor_visual_row_misses: 2,
                ..EditorViewportConversionStats::default()
            }
        );
    }

    #[test]
    fn conversion_cache_reuses_matching_helix_snapshot() {
        let base = EditorViewportConversionBaseKey {
            document_version: 1,
            text_len: 12,
            view_id: ViewId::default(),
            text_format: EditorViewportTextFormatKey::from(&TextFormat::default()),
        };
        let key = HelixViewportSnapshotCacheKey {
            base: base.clone(),
            view_position: ViewPosition::default(),
        };
        let snapshot = HelixViewportSnapshot {
            anchor_line: 2,
            vertical_offset: 1,
            top_visual_row: 3,
        };
        let mut cache = EditorViewportConversionCache::default();

        assert_eq!(cache.helix_snapshot(&key), None);
        cache.store_helix_snapshot(key.clone(), snapshot);
        assert_eq!(cache.helix_snapshot(&key), Some(snapshot));

        let changed_key = HelixViewportSnapshotCacheKey {
            view_position: ViewPosition {
                anchor: 8,
                vertical_offset: 0,
                horizontal_offset: 0,
            },
            ..key
        };
        assert_eq!(cache.helix_snapshot(&changed_key), None);
        assert_eq!(
            cache.stats(),
            EditorViewportConversionStats {
                helix_snapshot_hits: 1,
                helix_snapshot_misses: 2,
                ..EditorViewportConversionStats::default()
            }
        );
    }

    #[test]
    fn viewport_conversion_stats_count_misses_hits_and_scans() {
        let (document, view) = test_document_and_view("one\ntwo\nthree");
        let viewport = EditorViewport::new(px(20.0));
        let text_format = TextFormat::default();

        viewport.sync_from_helix_view(&document, &view, view.id, &text_format);
        viewport.plan_view_position(&document, &view, view.id, &text_format);
        viewport.document_cursor_visual_row(&document, &view, view.id, &text_format);

        assert_eq!(
            viewport.conversion_stats(),
            EditorViewportConversionStats {
                helix_snapshot_misses: 1,
                view_position_plan_misses: 1,
                cursor_visual_row_misses: 1,
                helix_snapshot_visual_scans: 1,
                view_position_char_scans: 1,
                cursor_visual_row_scans: 1,
                ..EditorViewportConversionStats::default()
            }
        );

        viewport.sync_from_helix_view(&document, &view, view.id, &text_format);
        viewport.plan_view_position(&document, &view, view.id, &text_format);
        viewport.document_cursor_visual_row(&document, &view, view.id, &text_format);

        assert_eq!(
            viewport.conversion_stats(),
            EditorViewportConversionStats {
                helix_snapshot_hits: 1,
                helix_snapshot_misses: 1,
                view_position_plan_hits: 1,
                view_position_plan_misses: 1,
                cursor_visual_row_hits: 1,
                cursor_visual_row_misses: 1,
                helix_snapshot_visual_scans: 1,
                view_position_char_scans: 1,
                cursor_visual_row_scans: 1,
            }
        );
    }

    #[test]
    fn view_position_for_top_visual_row_clamps_to_text() {
        let text = Rope::from("one\ntwo");
        let text_format = TextFormat::default();
        let annotations = default_annotations();

        let view_position =
            view_position_for_top_visual_row(text.slice(..), 1_000, 7, &text_format, &annotations);

        assert_eq!(text.char_to_line(view_position.anchor), 1);
        assert_eq!(view_position.vertical_offset, 0);
        assert_eq!(view_position.horizontal_offset, 7);
    }

    #[test]
    fn view_position_for_top_visual_row_clears_soft_wrap_horizontal_offset() {
        let text = "abcdef\nzz";
        let text_format = TextFormat {
            soft_wrap: true,
            viewport_width: 3,
            ..TextFormat::default()
        };
        let annotations = default_annotations();

        let view_position =
            view_position_for_top_visual_row(text.into(), 1, 7, &text_format, &annotations);

        assert_eq!(view_position.horizontal_offset, 0);
    }

    #[test]
    fn view_position_plan_reports_noop_for_matching_native_row() {
        let text = Rope::from("one\ntwo\nthree");
        let text_format = TextFormat::default();
        let annotations = default_annotations();
        let current_position = ViewPosition {
            anchor: text.line_to_char(1),
            vertical_offset: 0,
            horizontal_offset: 7,
        };

        let plan = view_position_plan_for_top_visual_row(
            text.slice(..),
            current_position,
            1,
            current_position.horizontal_offset,
            &text_format,
            &annotations,
        );

        assert!(!plan.changed);
        assert_eq!(plan.top_visual_row, 1);
        assert_eq!(plan.previous_view_position, current_position);
        assert_eq!(plan.view_position, current_position);
    }

    #[test]
    fn view_position_plan_maps_trailing_newline_eof_row() {
        let text = Rope::from("one\ntwo\n");
        let text_format = TextFormat::default();
        let annotations = default_annotations();
        let current_position = ViewPosition {
            anchor: 0,
            vertical_offset: 0,
            horizontal_offset: 0,
        };
        let eof_row = text.len_lines().saturating_sub(1);

        let plan = view_position_plan_for_top_visual_row(
            text.slice(..),
            current_position,
            eof_row,
            current_position.horizontal_offset,
            &text_format,
            &annotations,
        );

        assert!(plan.changed);
        assert_eq!(plan.top_visual_row, eof_row);
        assert_eq!(plan.view_position.anchor, text.len_chars());
        assert_eq!(text.char_to_line(plan.view_position.anchor), eof_row);
    }

    #[test]
    fn view_position_plan_clears_soft_wrap_horizontal_offset() {
        let text = Rope::from("abcdef\nzz");
        let text_format = TextFormat {
            soft_wrap: true,
            viewport_width: 3,
            ..TextFormat::default()
        };
        let annotations = default_annotations();
        let current_position = ViewPosition {
            anchor: 0,
            vertical_offset: 0,
            horizontal_offset: 7,
        };

        let plan = view_position_plan_for_top_visual_row(
            text.slice(..),
            current_position,
            0,
            current_position.horizontal_offset,
            &text_format,
            &annotations,
        );

        assert!(plan.changed);
        assert_eq!(plan.view_position.horizontal_offset, 0);
    }

    #[test]
    fn viewport_sync_view_position_updates_document_offset() {
        let (mut document, view) = test_document_and_view("one\ntwo\nthree\n");
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(240.0), px(20.0)), 4);
        viewport.sync_from_helix_top_visual_row(1);

        let synced =
            viewport.sync_view_position(&mut document, &view, view.id, &TextFormat::default());

        assert!(synced);
        assert_eq!(
            document
                .text()
                .char_to_line(document.view_offset(view.id).anchor),
            1
        );
    }

    #[test]
    fn viewport_sync_view_position_reports_noop_for_matching_offset() {
        let (mut document, view) = test_document_and_view("one\ntwo\nthree\n");
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(240.0), px(20.0)), 4);
        viewport.sync_from_helix_top_visual_row(1);
        document.set_view_offset(
            view.id,
            ViewPosition {
                anchor: document.text().line_to_char(1),
                vertical_offset: 0,
                horizontal_offset: 0,
            },
        );

        let synced =
            viewport.sync_view_position(&mut document, &view, view.id, &TextFormat::default());

        assert!(!synced);
        assert_eq!(
            document
                .text()
                .char_to_line(document.view_offset(view.id).anchor),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn surface_layout_sync_applies_native_horizontal_scroll_to_view_position() {
        let (mut editor, doc_id, view_id) =
            test_editor_with_text("abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz\n");
        let mut viewport = EditorViewport::new(px(20.0));
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(120.0), px(100.0)));
        let layout = EditorViewportSurfaceLayout {
            theme: None,
            bounds,
            cell_width: px(10.0),
            line_height: px(20.0),
            minimum_columns: 1,
            extra_gutter_columns: 0,
            scrolloff: Config::default().scrolloff,
            cursor_reveal: None,
        };

        viewport.sync_surface_layout(&mut editor, doc_id, view_id, layout);

        let scroll_update = viewport.scroll_to_horizontal_position_from_scrollbar(px(30.0));
        assert!(scroll_update.changed);
        assert!(viewport.has_pending_view_sync());

        let update = viewport
            .sync_surface_layout(&mut editor, doc_id, view_id, layout)
            .unwrap();
        let doc = editor.document(doc_id).unwrap();

        assert_eq!(update.view_position.horizontal_offset, 3);
        assert_eq!(doc.view_offset(view_id).horizontal_offset, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn surface_layout_render_position_clamps_stale_helix_anchor() {
        let (mut editor, doc_id, view_id) = test_editor_with_text("one\ntwo");
        let mut viewport = EditorViewport::new(px(20.0));
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(240.0), px(41.0)));

        {
            let doc = editor.document_mut(doc_id).unwrap();
            doc.set_view_offset(
                view_id,
                ViewPosition {
                    anchor: 1_000,
                    vertical_offset: 0,
                    horizontal_offset: 7,
                },
            );
        }

        let update = viewport
            .sync_surface_layout(
                &mut editor,
                doc_id,
                view_id,
                EditorViewportSurfaceLayout {
                    theme: None,
                    bounds,
                    cell_width: px(8.0),
                    line_height: px(20.0),
                    minimum_columns: 1,
                    extra_gutter_columns: 0,
                    scrolloff: Config::default().scrolloff,
                    cursor_reveal: None,
                },
            )
            .unwrap();

        let doc = editor.document(doc_id).unwrap();
        assert_eq!(doc.view_offset(view_id).anchor, 1_000);
        assert_eq!(
            update.view_position_plan.view_position,
            update.view_position
        );
        assert!(update.view_position.anchor <= doc.text().len_chars());
        assert_eq!(doc.text().char_to_line(update.view_position.anchor), 1);
        assert_eq!(update.view_position.horizontal_offset, 7);
    }

    #[test]
    fn document_cursor_visual_row_matches_unwrapped_line() {
        let (mut document, view) = test_document_and_view("one\ntwo\nthree");
        document.set_selection(view.id, Selection::single(5, 5));

        let row = document_cursor_visual_row(&document, &view, view.id, &TextFormat::default());

        assert_eq!(row, 1);
    }

    #[test]
    fn document_cursor_visual_row_uses_final_empty_line_at_trailing_newline_eof() {
        let (mut document, view) = test_document_and_view("one\n");
        document.set_selection(view.id, Selection::single(4, 4));

        let row = document_cursor_visual_row(&document, &view, view.id, &TextFormat::default());

        assert_eq!(row, 1);
    }

    #[test]
    fn document_cursor_visual_row_uses_soft_wrap_rows() {
        let (mut document, view) = test_document_and_view("abcdef\nzz");
        document.set_selection(view.id, Selection::single(7, 7));
        let text_format = TextFormat {
            soft_wrap: true,
            viewport_width: 3,
            ..TextFormat::default()
        };

        let row = document_cursor_visual_row(&document, &view, view.id, &text_format);

        assert!(row > 1);
    }
}
