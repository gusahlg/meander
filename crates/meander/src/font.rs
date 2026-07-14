//! Thin wrapper around a `fontdue::Font` with a bounded glyph cache.
//!
//! Meander intentionally does not embed a default font. You pick the bytes
//! (read from disk, embed with `include_bytes!`, fetch from fontconfig) and
//! hand them to [`Font::from_bytes`].
//!
//! Rasterised glyphs are cached internally so drawing the same string every
//! frame doesn't pay the rasterisation cost more than once per (character,
//! size) pair. The cache is `Arc<Mutex<…>>`-backed so a `Font` stays cheap to
//! clone and remains `Send + Sync`, and it is bounded: entries are evicted
//! least-recently-used once the cached bitmap bytes exceed a budget, so drawing
//! at ever-changing sizes cannot grow memory without limit.
//!
//! Measuring and drawing the same string repeats glyph lookups. To pay for the
//! layout once, [`Font::prepare`] returns a [`TextRun`] that both reports its
//! width and can be drawn via [`Canvas::draw_text_run`]. The simple
//! [`Canvas::text`] / [`Canvas::text_width`] methods are sugar over the same
//! machinery for one-off calls.
//!
//! [`Canvas::draw_text_run`]: crate::Canvas::draw_text_run
//! [`Canvas::text`]: crate::Canvas::text
//! [`Canvas::text_width`]: crate::Canvas::text_width

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fontdue::FontSettings;

use crate::error::{Error, Result};

/// Default cap on cached glyph-bitmap bytes before LRU eviction kicks in.
/// 8 MiB holds thousands of glyphs at typical UI sizes.
pub const DEFAULT_CACHE_BUDGET: usize = 8 * 1024 * 1024;

/// A reference-counted, immutable, rasterisable font face.
#[derive(Clone)]
pub struct Font {
    inner: Arc<fontdue::Font>,
    cache: Arc<Mutex<GlyphCache>>,
}

struct GlyphCache {
    slots: HashMap<GlyphKey, CacheSlot>,
    /// Sum of cached bitmap bytes (metrics-only slots count as 0).
    bytes: usize,
    /// Monotonic recency clock, bumped on every access.
    tick: u64,
    /// Byte budget; bitmaps beyond it trigger LRU eviction.
    budget: usize,
}

struct CacheSlot {
    entry: Arc<GlyphEntry>,
    last_used: u64,
    bytes: usize,
}

impl GlyphCache {
    fn new(budget: usize) -> Self {
        Self {
            slots: HashMap::new(),
            bytes: 0,
            tick: 0,
            budget,
        }
    }

    fn touch(&mut self, key: &GlyphKey) -> Option<Arc<GlyphEntry>> {
        self.tick += 1;
        let tick = self.tick;
        let slot = self.slots.get_mut(key)?;
        slot.last_used = tick;
        Some(slot.entry.clone())
    }

    fn insert(&mut self, key: GlyphKey, entry: Arc<GlyphEntry>) -> Arc<GlyphEntry> {
        self.tick += 1;
        let bytes = entry.bitmap.len();
        if let Some(old) = self.slots.remove(&key) {
            self.bytes -= old.bytes;
        }
        self.slots.insert(
            key,
            CacheSlot {
                entry: entry.clone(),
                last_used: self.tick,
                bytes,
            },
        );
        self.bytes += bytes;
        self.evict_to_budget();
        entry
    }

    /// Evict least-recently-used slots until the cached bytes fit the budget.
    /// Never evicts below one entry, so a single oversized glyph still caches.
    fn evict_to_budget(&mut self) {
        while self.bytes > self.budget && self.slots.len() > 1 {
            let Some(victim) = self
                .slots
                .iter()
                .min_by_key(|(_, s)| s.last_used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some(slot) = self.slots.remove(&victim) {
                self.bytes -= slot.bytes;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    ch: u32,
    size_bits: u32,
}

impl GlyphKey {
    fn new(ch: char, size_px: f32) -> Self {
        Self {
            ch: ch as u32,
            size_bits: size_px.to_bits(),
        }
    }
}

/// A cached glyph: its metrics plus, once rasterised, its coverage bitmap.
pub(crate) struct GlyphEntry {
    pub(crate) metrics: fontdue::Metrics,
    /// Coverage bitmap, `metrics.width * metrics.height` bytes. Empty for a
    /// zero-area glyph (e.g. space) or a metrics-only entry.
    pub(crate) bitmap: Vec<u8>,
    /// Whether `bitmap` is a valid rasterisation. A metrics-only entry (from a
    /// measurement lookup) is `false` and is upgraded on the first draw.
    pub(crate) rasterized: bool,
}

/// A laid-out run of glyphs at one size, ready to measure and draw without
/// touching the glyph cache again. Produced by [`Font::prepare`].
#[derive(Clone)]
pub struct TextRun {
    glyphs: Vec<RunGlyph>,
    advance: f32,
    size_px: f32,
}

#[derive(Clone)]
struct RunGlyph {
    entry: Arc<GlyphEntry>,
    /// Pen x at this glyph's start, relative to the run origin.
    pen: f32,
}

impl TextRun {
    /// Total advance width of the run, in physical pixels.
    pub fn width(&self) -> f32 {
        self.advance
    }

    /// The size this run was laid out at.
    pub fn size_px(&self) -> f32 {
        self.size_px
    }

    /// True when the run has no glyphs (empty string or an invalid size).
    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }

    pub(crate) fn glyphs(&self) -> impl Iterator<Item = (&GlyphEntry, f32)> {
        self.glyphs.iter().map(|g| (g.entry.as_ref(), g.pen))
    }
}

/// Clamp a caller-supplied font size to a finite positive value, or `None` if
/// it is NaN, infinite, or non-positive. Guards the cache key (which is the raw
/// float bits) against garbage and keeps rasterisation well-defined.
fn sanitize_size(size_px: f32) -> Option<f32> {
    (size_px.is_finite() && size_px > 0.0).then_some(size_px)
}

impl Font {
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let f = fontdue::Font::from_bytes(data, FontSettings::default())
            .map_err(|_| Error::Font("could not parse font bytes"))?;
        Ok(Self {
            inner: Arc::new(f),
            cache: Arc::new(Mutex::new(GlyphCache::new(DEFAULT_CACHE_BUDGET))),
        })
    }

    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes)
    }

    /// Vertical metrics for `px` pixels: (ascent, descent, line_gap). A
    /// non-finite or non-positive `px` yields a degenerate `(px, 0, 0)`.
    pub fn metrics(&self, px: f32) -> (f32, f32, f32) {
        let px = sanitize_size(px).unwrap_or(0.0);
        let m = self
            .inner
            .horizontal_line_metrics(px)
            .unwrap_or(fontdue::LineMetrics {
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

    /// Lay out `text` at `size_px` into a reusable [`TextRun`], rasterising each
    /// glyph once under a single cache lock. Measure it with [`TextRun::width`]
    /// and draw it with [`Canvas::draw_text_run`] — no further glyph lookups.
    ///
    /// [`Canvas::draw_text_run`]: crate::Canvas::draw_text_run
    pub fn prepare(&self, text: &str, size_px: f32) -> TextRun {
        let Some(size_px) = sanitize_size(size_px) else {
            return TextRun {
                glyphs: Vec::new(),
                advance: 0.0,
                size_px: 0.0,
            };
        };
        let mut glyphs = Vec::with_capacity(text.chars().count());
        let mut pen = 0.0;
        // One lock for the whole run.
        let mut cache = self.cache.lock().expect("font cache poisoned");
        for ch in text.chars() {
            let entry = self.rasterise_locked(&mut cache, ch, size_px);
            let advance = entry.metrics.advance_width;
            glyphs.push(RunGlyph { entry, pen });
            pen += advance;
        }
        TextRun {
            glyphs,
            advance: pen,
            size_px,
        }
    }

    /// Total advance width of `text` at `size_px`, metrics-only (no bitmaps),
    /// under a single cache lock. `0.0` for an invalid size.
    pub(crate) fn measure(&self, text: &str, size_px: f32) -> f32 {
        let Some(size_px) = sanitize_size(size_px) else {
            return 0.0;
        };
        let mut cache = self.cache.lock().expect("font cache poisoned");
        text.chars()
            .map(|ch| self.metrics_locked(&mut cache, ch, size_px).advance_width)
            .sum()
    }

    fn rasterise_locked(&self, cache: &mut GlyphCache, ch: char, size_px: f32) -> Arc<GlyphEntry> {
        let key = GlyphKey::new(ch, size_px);
        if let Some(entry) = cache.touch(&key) {
            if entry.rasterized {
                return entry;
            }
        }
        let (metrics, bitmap) = self.inner.rasterize(ch, size_px);
        let entry = Arc::new(GlyphEntry {
            metrics,
            bitmap,
            rasterized: true,
        });
        cache.insert(key, entry)
    }

    fn metrics_locked(&self, cache: &mut GlyphCache, ch: char, size_px: f32) -> fontdue::Metrics {
        let key = GlyphKey::new(ch, size_px);
        if let Some(entry) = cache.touch(&key) {
            return entry.metrics;
        }
        // Metrics-only: cheaper than a full rasterisation, and cached so a later
        // draw pays only the bitmap cost.
        let metrics = self.inner.metrics(ch, size_px);
        cache.insert(
            key,
            Arc::new(GlyphEntry {
                metrics,
                bitmap: Vec::new(),
                rasterized: false,
            }),
        );
        metrics
    }

    // --- cache controls -----------------------------------------------------

    /// Current cached glyph-bitmap byte total.
    pub fn cache_bytes(&self) -> usize {
        self.cache.lock().expect("font cache poisoned").bytes
    }

    /// Number of cached glyph slots (rasterised or metrics-only).
    pub fn cache_len(&self) -> usize {
        self.cache.lock().expect("font cache poisoned").slots.len()
    }

    /// The current LRU byte budget.
    pub fn cache_budget(&self) -> usize {
        self.cache.lock().expect("font cache poisoned").budget
    }

    /// Set the LRU byte budget, evicting immediately if the cache is over it.
    pub fn set_cache_budget(&self, budget: usize) {
        let mut cache = self.cache.lock().expect("font cache poisoned");
        cache.budget = budget;
        cache.evict_to_budget();
    }

    /// Drop every cached glyph.
    pub fn clear_cache(&self) {
        let mut cache = self.cache.lock().expect("font cache poisoned");
        cache.slots.clear();
        cache.bytes = 0;
    }
}

impl std::fmt::Debug for Font {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Font").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny valid TTF is awkward to embed, so the cache-mechanics tests drive
    // GlyphCache directly; the font-backed paths are covered by the example and
    // integration use.

    fn entry(bytes: usize, rasterized: bool) -> Arc<GlyphEntry> {
        Arc::new(GlyphEntry {
            metrics: fontdue::Metrics::default(),
            bitmap: vec![0u8; bytes],
            rasterized,
        })
    }

    fn key(ch: char, size: f32) -> GlyphKey {
        GlyphKey::new(ch, size)
    }

    #[test]
    fn sanitize_rejects_non_finite_and_non_positive() {
        assert_eq!(sanitize_size(14.0), Some(14.0));
        assert_eq!(sanitize_size(0.0), None);
        assert_eq!(sanitize_size(-3.0), None);
        assert_eq!(sanitize_size(f32::NAN), None);
        assert_eq!(sanitize_size(f32::INFINITY), None);
    }

    #[test]
    fn insert_tracks_byte_total() {
        let mut c = GlyphCache::new(1024);
        c.insert(key('a', 14.0), entry(100, true));
        c.insert(key('b', 14.0), entry(200, true));
        assert_eq!(c.bytes, 300);
        assert_eq!(c.slots.len(), 2);
    }

    #[test]
    fn reinserting_a_key_replaces_its_byte_count() {
        let mut c = GlyphCache::new(1024);
        c.insert(key('a', 14.0), entry(100, false));
        c.insert(key('a', 14.0), entry(250, true));
        assert_eq!(c.slots.len(), 1);
        assert_eq!(c.bytes, 250);
    }

    #[test]
    fn eviction_drops_least_recently_used_first() {
        let mut c = GlyphCache::new(250);
        c.insert(key('a', 14.0), entry(100, true));
        c.insert(key('b', 14.0), entry(100, true));
        // Touch 'a' so 'b' becomes the LRU victim.
        c.touch(&key('a', 14.0));
        c.insert(key('c', 14.0), entry(100, true)); // 300 > 250 → evict one
        assert!(c.slots.contains_key(&key('a', 14.0)));
        assert!(!c.slots.contains_key(&key('b', 14.0)));
        assert!(c.slots.contains_key(&key('c', 14.0)));
        assert!(c.bytes <= 250);
    }

    #[test]
    fn eviction_keeps_at_least_one_entry_even_when_oversized() {
        let mut c = GlyphCache::new(10);
        c.insert(key('a', 14.0), entry(1000, true));
        assert_eq!(c.slots.len(), 1);
        assert!(c.bytes > c.budget);
    }

    #[test]
    fn shrinking_budget_evicts_immediately() {
        let mut c = GlyphCache::new(1000);
        c.insert(key('a', 14.0), entry(300, true));
        c.touch(&key('a', 14.0));
        c.insert(key('b', 14.0), entry(300, true));
        c.budget = 300;
        c.evict_to_budget();
        assert!(c.bytes <= 300);
        assert_eq!(c.slots.len(), 1);
    }

    #[test]
    fn metrics_only_entries_cost_no_bytes() {
        let mut c = GlyphCache::new(1024);
        c.insert(key('a', 14.0), entry(0, false));
        assert_eq!(c.bytes, 0);
        assert_eq!(c.slots.len(), 1);
    }

    #[test]
    fn empty_run_reports_zero_width() {
        let run = TextRun {
            glyphs: Vec::new(),
            advance: 0.0,
            size_px: 0.0,
        };
        assert!(run.is_empty());
        assert_eq!(run.width(), 0.0);
    }
}
