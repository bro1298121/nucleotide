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
}

impl Default for EditorScrollConfig {
    fn default() -> Self {
        Self {
            smooth_scrolling: true,
            wheel_glide: true,
        }
    }
}

/// `true`, for `#[serde(default = "...")]` on a bool that defaults to on.
const fn default_true() -> bool {
    true
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
