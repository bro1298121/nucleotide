// ABOUTME: Core editor data types
// ABOUTME: Pure data structures for editor state

use serde::{Deserialize, Serialize};

/// Diagnostic severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Hint,
    Info,
    Warning,
    Error,
}

/// Editor status information
#[derive(Debug, Clone)]
pub struct EditorStatus {
    pub status: String,
    pub severity: Severity,
}

/// Editor viewport motion configuration.
///
/// Despite the name, this struct also carries the cursor-trail keys
/// (`cursor_trail`, `cursor_trail_size`, `cursor_animation_length`,
/// `cursor_short_animation_length`). Deliberately kept in one shared GPUI
/// global for all cursor/viewport motion: renaming the struct would churn
/// every user for no behaviour gain. Do not "fix" the name by scattering the
/// trail keys into a new struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorScrollConfig {
    pub smooth_scrolling: bool,
    /// Smooth mouse-wheel scrolling, including the glide after a gesture stops.
    ///
    /// Each notch eases onto an accumulated destination, and a gesture that goes
    /// idle adds a short extra glide on top. Horizontal wheel scrolling stays 1:1
    /// and instant. Requires the tween engine (`smooth_scrolling`) to be on,
    /// because both the easing and the glide *are* tweens.
    #[serde(default = "default_true")]
    pub wheel_glide: bool,
    /// Animate the cursor with a short trail as it moves and scrolls.
    #[serde(default = "default_true")]
    pub cursor_trail: bool,
    /// Cursor trail stretch, `0.0..=1.0`; larger means more trail.
    #[serde(default = "default_cursor_trail_size")]
    pub cursor_trail_size: f32,
    /// Cursor trail glide length in seconds, `> 0`, capped at 0.5.
    #[serde(default = "default_cursor_animation_length")]
    pub cursor_animation_length: f32,
    /// Cursor trail short-hop length in seconds, `> 0` and at most
    /// `cursor_animation_length`.
    #[serde(default = "default_cursor_short_animation_length")]
    pub cursor_short_animation_length: f32,
}

impl Default for EditorScrollConfig {
    fn default() -> Self {
        Self {
            smooth_scrolling: true,
            wheel_glide: true,
            cursor_trail: true,
            cursor_trail_size: 0.9,
            cursor_animation_length: 0.120,
            cursor_short_animation_length: 0.035,
        }
    }
}

/// `true`, for `#[serde(default = "...")]` on a bool that defaults to on.
const fn default_true() -> bool {
    true
}

/// `0.9`, for `#[serde(default = "...")]` on the cursor trail size.
const fn default_cursor_trail_size() -> f32 {
    0.9
}

/// `0.120`, for `#[serde(default = "...")]` on the cursor trail glide length.
const fn default_cursor_animation_length() -> f32 {
    0.120
}

/// `0.035`, for `#[serde(default = "...")]` on the cursor trail short-hop length.
const fn default_cursor_short_animation_length() -> f32 {
    0.035
}

#[cfg(feature = "gpui-bridge")]
impl gpui::Global for EditorScrollConfig {}

#[cfg(feature = "helix-bridge")]
impl From<helix_core::diagnostic::Severity> for Severity {
    fn from(s: helix_core::diagnostic::Severity) -> Self {
        match s {
            helix_core::diagnostic::Severity::Hint => Severity::Hint,
            helix_core::diagnostic::Severity::Info => Severity::Info,
            helix_core::diagnostic::Severity::Warning => Severity::Warning,
            helix_core::diagnostic::Severity::Error => Severity::Error,
        }
    }
}
