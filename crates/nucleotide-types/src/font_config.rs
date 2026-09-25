// ABOUTME: Font configuration types
// ABOUTME: Pure data structures for font settings

use crate::config::FontWeight;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[cfg(feature = "gpui-bridge")]
use gpui::FontFeatures;

/// OpenType feature settings for a font, keyed by four-character feature tag.
///
/// This is deliberately stored in a gpui-independent form so `nucleotide-types` stays free of
/// gpui when the `gpui-bridge` feature is disabled; it is translated to [`gpui::FontFeatures`]
/// at the boundary.
///
/// ```toml
/// [editor.font]
/// features = { calt = true, liga = true, clig = true }
/// ```
///
/// The commonly requested programming-ligature tags are `calt` (contextual alternates, which
/// Fira Code uses for most of its ligatures), `liga` (standard ligatures) and `clig`
/// (contextual ligatures).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FontFeatureSettings(pub BTreeMap<String, bool>);

impl FontFeatureSettings {
    /// Whether no feature overrides are configured.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Enable the common programming-ligature tags.
    ///
    /// Equivalent to `features = { calt = true, liga = true, clig = true }`.
    pub fn ligatures() -> Self {
        let mut map = BTreeMap::new();
        map.insert("calt".to_string(), true);
        map.insert("clig".to_string(), true);
        map.insert("liga".to_string(), true);
        Self(map)
    }

    /// Whether `tag` is explicitly enabled by these settings.
    pub fn is_enabled(&self, tag: &str) -> Option<bool> {
        self.0.get(tag).copied()
    }
}

/// OpenType tags are exactly four ASCII alphanumeric characters.
fn is_valid_font_feature_tag(tag: &str) -> bool {
    tag.len() == 4 && tag.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(feature = "gpui-bridge")]
impl FontFeatureSettings {
    /// Convert to GPUI's `FontFeatures`, dropping tags that are not four ASCII alphanumeric
    /// characters so invalid configuration cannot reach the text backend.
    pub fn to_gpui_font_features(&self) -> FontFeatures {
        let features: Vec<(String, u32)> = self
            .0
            .iter()
            .filter(|(tag, _)| is_valid_font_feature_tag(tag))
            .map(|(tag, enabled)| (tag.clone(), u32::from(*enabled)))
            .collect();
        FontFeatures(std::sync::Arc::new(features))
    }
}

/// Font descriptor - lightweight representation of a font
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Font {
    pub family: String,
    pub weight: FontWeight,
    pub style: FontStyle,
    /// OpenType features applied to this font.
    #[serde(default)]
    pub features: FontFeatureSettings,
}

/// Font style
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FontStyle {
    Normal,
    Italic,
    Oblique,
}

/// Font settings for the application
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FontSettings {
    pub fixed_font: Font,
    pub var_font: Font,
}

#[cfg(feature = "gpui-bridge")]
impl gpui::Global for FontSettings {}

/// UI font configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiFontConfig {
    pub family: String,
    pub size: f32,
    pub weight: FontWeight,
}

#[cfg(feature = "gpui-bridge")]
impl gpui::Global for UiFontConfig {}

/// Editor font configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditorFontConfig {
    pub family: String,
    pub size: f32,
    pub weight: FontWeight,
    pub line_height: f32,
    /// OpenType features applied to the editor font (for example programming ligatures).
    #[serde(default)]
    pub features: FontFeatureSettings,
}

#[cfg(feature = "gpui-bridge")]
impl gpui::Global for EditorFontConfig {}

#[cfg(feature = "gpui-bridge")]
impl From<Font> for gpui::Font {
    fn from(font: Font) -> Self {
        gpui::Font {
            family: font.family.into(),
            weight: font.weight.into(),
            style: match font.style {
                FontStyle::Normal => gpui::FontStyle::Normal,
                FontStyle::Italic => gpui::FontStyle::Italic,
                FontStyle::Oblique => gpui::FontStyle::Oblique,
            },
            #[cfg(feature = "gpui-bridge")]
            features: font.features.to_gpui_font_features(),
            #[cfg(not(feature = "gpui-bridge"))]
            features: Default::default(),
            fallbacks: None,
        }
    }
}
