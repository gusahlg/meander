//! Thin wrapper around a `fontdue::Font`.
//!
//! Meander intentionally does not embed a default font. You pick the bytes
//! (read from disk, embed with `include_bytes!`, fetch from fontconfig) and
//! hand them to [`Font::from_bytes`].

use std::sync::Arc;

use fontdue::FontSettings;

use crate::error::{Error, Result};

/// A reference-counted, immutable, rasterisable font face.
#[derive(Clone)]
pub struct Font {
    inner: Arc<fontdue::Font>,
}

impl Font {
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let f = fontdue::Font::from_bytes(data, FontSettings::default())
            .map_err(|_| Error::Font("could not parse font bytes"))?;
        Ok(Self { inner: Arc::new(f) })
    }

    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes)
    }

    /// Vertical metrics for `px` pixels: (ascent, descent, line_gap).
    pub fn metrics(&self, px: f32) -> (f32, f32, f32) {
        let m = self.inner.horizontal_line_metrics(px).unwrap_or(fontdue::LineMetrics {
            ascent: px,
            descent: 0.0,
            line_gap: 0.0,
            new_line_size: px,
        });
        (m.ascent, m.descent, m.line_gap)
    }

    pub fn line_height(&self, px: f32) -> f32 {
        let (a, d, g) = self.metrics(px);
        a - d + g
    }

    pub(crate) fn raw(&self) -> &fontdue::Font {
        &self.inner
    }
}

impl std::fmt::Debug for Font {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Font").finish_non_exhaustive()
    }
}
