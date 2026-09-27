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
        let (changed, crossed_visual_rows) = self.scroll.scroll_by_delta(delta);

        ViewportScrollUpdate {
            changed,
            crossed_visual_rows,
            top_visual_row: self.top_visual_row(),
            offset_within_row: self.offset_within_row(),
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
        let tween_destination_row =
            self.scroll.pixels_to_anchor(self.scroll.destination_position().y);

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
                let margin = scrolloff.min(visible_rows.saturating_sub(1) / 2);
                let top_visual_row = self.top_visual_row();
                let lower_bound = top_visual_row.saturating_add(margin);
                let upper_bound =
                    top_visual_row.saturating_add(visible_rows.saturating_sub(margin));

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

        if let Some(target_top) = target_top {
            // Deliberately uses the instant `set_scroll_position` path: a cursor
            // reveal has to be correct on the very next frame, so it never goes
            // through the smooth tween.
            self.scroll.set_scroll_position(
                point(old_position.x, self.scroll.anchor_to_pixels(target_top)),
                "reveal_visual_row",
            );
        }

        // Result log. `target_top` is the row the band chose, which is the row
        // the instant jump above moved to. `None` means the band judged the
        // cursor already visible and nothing moved.
        trace!(
            reason = "reveal_visual_row",
            old_top_visual_row,
            tween_active,
            tween_destination_row,
            visual_row,
            target_top = ?target_top,
            new_top_visual_row = self.top_visual_row(),
            "EditorViewport reveal applied"
        );

        let new_position = self.scroll_position();
        let new_top_visual_row = self.top_visual_row();

        ViewportScrollUpdate {
            changed: old_position != new_position,
            crossed_visual_rows: new_top_visual_row as isize - old_top_visual_row as isize,
            top_visual_row: new_top_visual_row,
            offset_within_row: self.offset_within_row(),
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
    /// covers a wheel gesture that is waiting out its idle window.
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

    #[test]
    fn viewport_reports_subrow_wheel_scroll() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(400.0)), 100);

        let update = viewport.scroll_by_delta(point(px(0.0), px(-5.0)));

        assert!(update.changed);
        assert_eq!(update.crossed_visual_rows, 0);
        assert_eq!(update.top_visual_row, 0);
        assert_eq!(update.offset_within_row, px(5.0));
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

    /// Reproduces the traced `page_down` failure and pins the mechanism the
    /// workspace fix depends on.
    ///
    /// The trace: a page scroll arms a tween, `sync_cursor_after_native_page_scroll`
    /// moves the Helix cursor into the *destination* page, `handle_selection_changed`
    /// arms a `Scrolloff` reveal, and the next painted frame consumes that reveal
    /// against the still-mid-tween `top_visual_row` and takes the tween down.
    ///
    /// `Workspace::handle_viewport_scroll` now calls `clear_cursor_reveal_request`
    /// after the cursor sync, i.e. it *discards* the armed request. This asserts
    /// that discarding is sufficient: the tween must run to its destination, while
    /// applying the very same request one frame earlier kills it.
    ///
    /// This does not cover the workspace wiring itself — `handle_viewport_scroll`
    /// needs a `Context<Workspace>` and cannot be driven headlessly. It covers the
    /// viewport contract the wiring relies on.
    #[test]
    fn page_scroll_reveal_kills_the_tween_only_when_it_is_applied() {
        // Applying the armed reveal is what kills the tween.
        let mut killed = EditorViewport::new(px(20.0));
        killed.set_layout(px(20.0), size(px(800.0), px(800.0)), 100);
        killed.set_smooth_scrolling(true);
        let update = killed.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));
        assert_eq!(update.top_visual_row, 40);
        assert!(killed.scroll_animation_active());

        killed.request_cursor_reveal(EditorCursorReveal::Scrolloff);
        assert!(killed.advance_scroll_animation_at(Instant::now() + Duration::from_millis(20)));
        let armed = killed
            .take_cursor_reveal_request()
            .expect("armed reveal from the selection change");
        killed.reveal_visual_row(45, armed, 5);

        assert!(!killed.scroll_animation_active());

        // Discarding the same request — what `clear_cursor_reveal_request` does —
        // leaves the tween running to its destination.
        let mut survived = EditorViewport::new(px(20.0));
        survived.set_layout(px(20.0), size(px(800.0), px(800.0)), 100);
        survived.set_smooth_scrolling(true);
        let update = survived.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));
        assert_eq!(update.top_visual_row, 40);
        assert!(survived.scroll_animation_active());

        survived.request_cursor_reveal(EditorCursorReveal::Scrolloff);
        assert_eq!(
            survived.take_cursor_reveal_request(),
            Some(EditorCursorReveal::Scrolloff),
            "the selection change armed a reveal"
        );
        assert!(survived.scroll_animation_active());

        assert!(survived.advance_scroll_animation_at(Instant::now() + Duration::from_millis(500)));

        assert_eq!(survived.top_visual_row(), 40);
        assert!(!survived.scroll_animation_active());
    }

    /// KNOWN-FAILURE CHARACTERIZATION — pins current, incorrect behaviour.
    ///
    /// `reveal_visual_row` derives its `Scrolloff` band from `top_visual_row()`,
    /// which during a tween is the *mid-tween* row rather than the destination
    /// row. The traced `page_down` case lands here: a 40-row page scroll puts the
    /// destination at row 40 and the cursor at destination + scrolloff = 45, but
    /// on the first painted frame the live row is still small, so the band is
    /// computed around the wrong origin, 45 is judged out of band, and the
    /// viewport is dragged back to roughly row 11 instead of row 40.
    ///
    /// The next phase fixes this (read the band origin from `destination_position()`
    /// and retarget instead of cancelling). It is pinned here so that fix has a
    /// red test to turn green. Do not correct the expected values below without
    /// fixing the behaviour they describe.
    #[test]
    fn known_failure_reveal_band_is_computed_from_the_mid_tween_row() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(800.0)), 100);
        viewport.set_smooth_scrolling(true);

        let update = viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));
        assert_eq!(update.top_visual_row, 40, "destination top visual row");

        assert!(viewport.advance_scroll_animation_at(Instant::now() + Duration::from_millis(20)));
        let live_row = viewport.top_visual_row();
        assert!(
            live_row < 40,
            "the tween should still be mid-flight, live row {live_row}"
        );

        // 45 is where the cursor really is: destination (40) + scrolloff (5).
        let reveal = viewport.reveal_visual_row(45, EditorCursorReveal::Scrolloff, 5);

        assert!(reveal.changed);
        // Wrong on purpose. The band origin is `live_row` (the mid-tween row, 5),
        // not the destination (40), so the *absolute* row the reveal lands on is
        // `cursor + margin + 1 - visible_rows` = 45 + 5 + 1 - 40 = 11 — 29 rows
        // short of the destination. The correct target is row 40.
        assert_eq!(viewport.top_visual_row(), 11);
        assert!(
            viewport.top_visual_row() < 40,
            "the reveal should miss the destination row 40 entirely, not land on it"
        );
        // The reveal still cancels the tween, which is exactly why the workspace
        // fix has to clear the request before it is ever applied.
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

    /// Fixture for the wheel-glide tests: 20px rows, a 400x800 viewport and 100
    /// content rows, so `max_scroll_offset().height = 100 * 20 - 800 = 1200px`
    /// of travel. Every glide target below stays well inside it, so nothing is
    /// clamped by the scroll range and the assertions are pure glide arithmetic.
    ///
    /// Both switches are on: the glide is gated on the tween engine
    /// (`smooth_scrolling`) as well as on its own key, because a glide *is* a
    /// tween.
    fn wheel_glide_viewport() -> EditorViewport {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(400.0), px(800.0)), 100);
        viewport.set_smooth_scrolling(true);
        viewport.set_wheel_glide(true);
        viewport
    }

    /// A single wheel event must not glide, and the frame loop must stop.
    ///
    /// One deliberate notch is a precision movement, not a flick. A glide on top
    /// of it would move the viewport further than the user asked for, so the
    /// event count is a gate in its own right, independent of travel.
    ///
    /// Arithmetic (one event of `delta.y = -40px` on a 0px start):
    ///  - 1:1 puts the position at `0 - (-40) = 40px`, inside row 2.
    ///  - accumulated = `-40px`, so `|accumulated| = 40 < ARM_MIN_PX = 72`, and
    ///    `events = 1 < ARM_MIN_EVENTS = 2`. Both gates fail, so no glide.
    ///  - `start` is taken *after* the wheel event, so `start + 90ms` is at
    ///    least `GESTURE_IDLE_MS` past the recorded timestamp. Sampling at
    ///    `start + 50ms` is therefore unambiguously still inside the window.
    ///
    /// The `!scroll_needs_frames()` assertion at the end is the load-bearing one:
    /// the gesture has to have been dropped, not merely declined, or the render
    /// loop would request frames forever.
    #[test]
    fn single_wheel_event_glides_nothing_and_stops_the_frame_loop() {
        let viewport = wheel_glide_viewport();
        assert!(viewport.wheel_glide_enabled());

        viewport.scroll_by_delta(point(px(0.0), px(-40.0)));
        assert_eq!(
            viewport.scroll_position(),
            point(px(0.0), px(40.0)),
            "the wheel must track 1:1 with no delay"
        );

        let start = Instant::now();
        // Inside the idle window: nothing is armed, and the loop has to keep
        // running so the gesture can still be decided.
        assert!(!viewport.advance_wheel_glide(start + Duration::from_millis(50)));
        assert!(!viewport.scroll_animation_active());
        assert!(viewport.scroll_needs_frames());

        let arm = start + Duration::from_millis(90);
        assert!(!viewport.advance_wheel_glide(arm));
        assert!(!viewport.scroll_animation_active());
        assert_eq!(
            viewport.scroll_position(),
            point(px(0.0), px(40.0)),
            "the declined gesture must leave the 1:1 position exactly as it was"
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

    /// A multi-event gesture that clears both gates arms exactly one glide,
    /// tweened over exactly `GLIDE_MS`.
    ///
    /// Sign convention: a negative `delta.y` scrolls DOWN, and a larger scroll
    /// position also means further down. The accumulator is kept in the wheel's
    /// sign space and negated into the position's, so a downward gesture glides
    /// further DOWN. Getting that negation wrong is invisible to an arithmetic
    /// check and very visible to a user, so the direction is pinned here.
    ///
    /// Arithmetic (four events of `delta.y = -50px` on a 0px start):
    ///  - 1:1 puts the position at `4 * 50 = 200px`, row 10.
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
        for _ in 0..4 {
            viewport.scroll_by_delta(point(px(0.0), px(-50.0)));
        }
        assert_eq!(viewport.scroll_position().y, px(200.0));

        let start = Instant::now();
        let arm = start + Duration::from_millis(90);
        assert!(viewport.advance_wheel_glide(arm));
        assert!(viewport.scroll_animation_active());
        // Arming does not move anything: the glide is on top of the 1:1 position.
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
            "the glide did not land on clamp(200 * 0.25, ±120) = 50px past the 1:1 position"
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
    /// positive one scrolls UP. Confirmed by `single_wheel_event_does_not_glide`,
    /// where `-40` lands the position at `+40px`.
    ///
    /// The gesture is parked at row 20 (`400px`) on purpose. A down-then-up flick
    /// nets to zero 1:1 travel, so the glide has to be the only thing that moves the
    /// view; if the start were `0px` the post-flip glide would point *up* and be
    /// clamped away at the document top, leaving nothing observable to assert.
    ///
    /// Arithmetic (`-100, -100, +100, +100` from `400px`):
    ///  - 1:1 position: down 200, up 200, so `400 - 200 + 200 = 400px`, row 20.
    ///  - accumulated after the first two events: `-200px`, `events = 2`.
    ///  - the third event flips the sign, so the accumulator is dropped and
    ///    restarted: `+100px`, `events = 1`. Netting would have left `-100px`.
    ///  - the fourth event keeps the sign: `+200px`, `events = 2`.
    ///  - `+200px` of accumulated travel means UP, so glide = `+50px` and the
    ///    tween is `400px -> 350px` over 140ms.
    ///
    /// So the assertion below is the whole point: netting would have produced
    /// `|accumulated| = 0` and *no* glide, leaving the position at 400px.
    #[test]
    fn direction_flip_resets_the_wheel_accumulator_instead_of_netting() {
        let viewport = wheel_glide_viewport();
        viewport.sync_from_helix_top_visual_row(20);
        assert_eq!(viewport.scroll_position().y, px(400.0));
        for delta in [-100.0, -100.0, 100.0, 100.0] {
            viewport.scroll_by_delta(point(px(0.0), px(delta)));
        }
        assert_eq!(viewport.scroll_position().y, px(400.0));

        let start = Instant::now();
        let arm = start + Duration::from_millis(90);
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

    /// A new wheel event kills an in-flight glide and resumes 1:1 from wherever
    /// the view actually is.
    ///
    /// The user's wheel is the only live input here, so it has to win
    /// immediately — including the "take over mid-flight" case, where resuming
    /// from the glide's *target* instead of its live position would teleport the
    /// viewport backwards by half the glide.
    ///
    /// Arithmetic (reuses the four-event gesture, then a `-10px` event):
    ///  - glide armed at `arm`: `200px -> 250px` over 140ms, continuing downward
    ///    because the accumulated wheel travel was `-200px` and the glide negates
    ///    into the position's sign space.
    ///  - at `arm + 70ms` the live position is `237.5px` (see the derivation in
    ///    `significant_wheel_gesture_arms_a_glide_of_the_expected_size_and_duration`).
    ///  - the new event cancels the tween and applies 1:1 from `237.5`, so the
    ///    position is `237.5 + 10 = 247.5px`. Targeting 250 instead would have
    ///    landed on `250 + 10 = 260px`.
    ///  - the new gesture is a fresh accumulator (1 event, `10px` of travel), so
    ///    it is below both gates, and the frame loop stops again.
    #[test]
    fn new_wheel_event_cancels_an_in_flight_glide_and_resumes_one_to_one() {
        let viewport = wheel_glide_viewport();
        for _ in 0..4 {
            viewport.scroll_by_delta(point(px(0.0), px(-50.0)));
        }
        let start = Instant::now();
        let arm = start + Duration::from_millis(90);
        assert!(viewport.advance_wheel_glide(arm));
        assert!(viewport.advance_scroll_animation_at(arm + Duration::from_millis(70)));
        assert_eq!(viewport.scroll_position().y, px(237.5));

        viewport.scroll_by_delta(point(px(0.0), px(-10.0)));

        assert!(
            !viewport.scroll_animation_active(),
            "the new wheel event left the glide tween in flight"
        );
        assert_eq!(
            viewport.scroll_position().y,
            px(247.5),
            "1:1 did not resume from the live position"
        );
        // The tween is dead, not merely unflagged: sampling far past its
        // deadline must move nothing.
        assert!(!viewport.advance_scroll_animation_at(arm + Duration::from_millis(500)));
        assert_eq!(viewport.scroll_position().y, px(247.5));

        // The replacement gesture is below both gates, so the loop still ends.
        assert!(viewport.scroll_needs_frames());
        let restart = Instant::now();
        assert!(!viewport.advance_wheel_glide(restart + Duration::from_millis(90)));
        assert!(!viewport.scroll_needs_frames());
        assert_eq!(viewport.scroll_position().y, px(247.5));
    }

    /// A hard fling is capped at `MAX_PX`, in both directions.
    ///
    /// The cap is the only thing standing between a violent wheel and a viewport
    /// that keeps travelling after the user's hand has stopped, so both signs
    /// are pinned. It is applied *after* `RATIO`, which is why the accumulation
    /// has to reach `120 / 0.25 = 480px` before it can saturate.
    ///
    /// Sign convention: negative `delta.y` is DOWN, and a larger position is
    /// further down. So a downward fling glides to a LARGER position.
    ///
    /// Arithmetic (six events of 100px each; scroll range is 0..1200px):
    ///  - down: 1:1 position `600px`; accumulated `-600px`, which clears both
    ///    gates; `clamp(-(-600) * 0.25, ±120) = clamp(150, ±120) = 120px`, so the
    ///    tween is `600px -> 720px`. Without the cap it would have been 750px.
    ///  - up: parked at row 50 (`1000px`) rather than near the top, because
    ///    600px of upward travel has to fit below the position-0 clamp;
    ///    1:1 position `1000 - 600 = 400px`, accumulated `+600px`,
    ///    `clamp(-(600) * 0.25, ±120) = clamp(-150, ±120) = -120px`, so the tween
    ///    is `400px -> 280px`, still inside the scroll range.
    #[test]
    fn wheel_glide_is_clamped_to_max_px_in_both_directions() {
        let down = wheel_glide_viewport();
        for _ in 0..6 {
            down.scroll_by_delta(point(px(0.0), px(-100.0)));
        }
        assert_eq!(down.scroll_position().y, px(600.0));
        let down_start = Instant::now();
        let down_arm = down_start + Duration::from_millis(90);
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
        for _ in 0..6 {
            up.scroll_by_delta(point(px(0.0), px(100.0)));
        }
        assert_eq!(up.scroll_position().y, px(400.0));
        let up_start = Instant::now();
        let up_arm = up_start + Duration::from_millis(90);
        assert!(up.advance_wheel_glide(up_arm));
        assert!(up.advance_scroll_animation_at(up_arm + Duration::from_millis(140)));
        assert_eq!(
            up.scroll_position().y,
            px(280.0),
            "the upward glide was not capped at 120px"
        );
    }

    /// With the feature off, a gesture is exactly today's behaviour: 1:1, no
    /// gesture recorded, no glide, and no frames requested.
    ///
    /// The 1:1 position is the same number the enabled fixture produces for the
    /// same four events (200px), which is the actual claim: the flag changes the
    /// tail and nothing else. And because nothing is recorded,
    /// `scroll_needs_frames()` never becomes true, so the frame loop is never
    /// started in the first place — there is no window during which it could fail
    /// to stop.
    #[test]
    fn disabled_wheel_glide_glides_nothing_and_requests_no_frames() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(400.0), px(800.0)), 100);
        viewport.set_smooth_scrolling(true);
        viewport.set_wheel_glide(false);
        assert!(!viewport.wheel_glide_enabled());

        for _ in 0..4 {
            viewport.scroll_by_delta(point(px(0.0), px(-50.0)));
        }
        assert_eq!(viewport.scroll_position().y, px(200.0));

        let start = Instant::now();
        assert!(
            !viewport.scroll_needs_frames(),
            "a disabled glide still started the frame loop"
        );
        assert!(!viewport.advance_wheel_glide(start + Duration::from_millis(90)));
        assert!(!viewport.scroll_needs_frames());
        assert!(!viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position().y, px(200.0));

        // No accumulator survived the disabled window, so nothing can fire later.
        assert!(!viewport.advance_wheel_glide(start + Duration::from_millis(500)));
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

    #[test]
    fn viewport_wheel_delta_cancels_an_in_flight_scroll_animation() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(100.0), px(100.0)), 50);
        viewport.set_smooth_scrolling(true);
        viewport.apply_scroll_request(EditorViewportScrollRequest::VisualPages(1));
        assert!(viewport.scroll_animation_active());

        let update = viewport.scroll_by_delta(point(px(0.0), px(-5.0)));

        assert!(update.changed);
        assert!(!viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position(), point(px(0.0), px(5.0)));
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

    #[test]
    fn viewport_reveal_visual_row_stays_instant_with_smooth_scrolling() {
        let mut viewport = EditorViewport::new(px(20.0));
        viewport.set_layout(px(20.0), size(px(800.0), px(100.0)), 100);
        viewport.set_smooth_scrolling(true);

        let update = viewport.reveal_visual_row(20, EditorCursorReveal::Top, 0);

        assert!(update.changed);
        assert!(!viewport.scroll_animation_active());
        assert_eq!(viewport.scroll_position().y, px(400.0));
        assert_eq!(viewport.top_visual_row(), 20);
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
