//! Layer-shell surface configuration.

use bitflags::bitflags;

use crate::output::OutputId;

bitflags! {
    /// Which edges of the output a layer surface should stick to.
    ///
    /// Two opposite anchors imply the surface is stretched between them; all
    /// four anchors at once means "fill the work area minus exclusive zones".
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Anchor: u32 {
        const TOP    = 0b0001;
        const BOTTOM = 0b0010;
        const LEFT   = 0b0100;
        const RIGHT  = 0b1000;
    }
}

/// Z-stack position of the surface relative to normal toplevel windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    /// Behind every window. Wallpapers live here.
    Background,
    /// Above the background but below normal windows.
    Bottom,
    /// Above normal windows. Bars and panels live here.
    Top,
    /// Above everything, including fullscreen windows. Notifications and
    /// overlays live here.
    Overlay,
}

/// How a surface participates in keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyboardInteractivity {
    /// No keyboard input — the compositor never routes keys here.
    None,
    /// Steals keyboard focus while visible (launchers, modal pickers).
    Exclusive,
    /// Can receive focus when the user clicks on it.
    OnDemand,
}

/// Opaque handle to a layer surface registered with an [`App`](crate::App).
///
/// Cheap to copy; pass it around freely to identify which surface an event
/// belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub(crate) u32);

impl SurfaceId {
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Builder for [`App::layer_surface`](crate::App::layer_surface).
///
/// Every field has a default that matches the wlr-layer-shell defaults; you
/// only set what you care about.
pub struct LayerSurfaceBuilder<'a> {
    pub(crate) app: &'a mut crate::App,
    pub(crate) namespace: String,
    pub(crate) layer: Layer,
    pub(crate) anchor: Anchor,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) exclusive_zone: i32,
    pub(crate) margin_top: i32,
    pub(crate) margin_right: i32,
    pub(crate) margin_bottom: i32,
    pub(crate) margin_left: i32,
    pub(crate) interactivity: KeyboardInteractivity,
    pub(crate) output: Option<OutputId>,
}

impl<'a> LayerSurfaceBuilder<'a> {
    /// Identifier the compositor uses to tag the surface (e.g. "bar",
    /// "notifications"). Visible in `lswt`-style debug tools.
    pub fn namespace(mut self, s: impl Into<String>) -> Self {
        self.namespace = s.into();
        self
    }

    pub fn layer(mut self, l: Layer) -> Self {
        self.layer = l;
        self
    }

    pub fn anchor(mut self, a: Anchor) -> Self {
        self.anchor = a;
        self
    }

    /// Requested size in logical (scale-1) pixels. Pass `0` for a dimension to
    /// let the compositor pick it based on anchor edges (e.g. width 0 +
    /// LEFT|RIGHT means "as wide as the output").
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// How many pixels of the output edge this surface reserves for itself.
    /// `-1` means the surface does not reserve space and is allowed to overlap
    /// other layer surfaces' exclusive zones.
    pub fn exclusive_zone(mut self, z: i32) -> Self {
        self.exclusive_zone = z;
        self
    }

    pub fn margin(mut self, top: i32, right: i32, bottom: i32, left: i32) -> Self {
        self.margin_top = top;
        self.margin_right = right;
        self.margin_bottom = bottom;
        self.margin_left = left;
        self
    }

    pub fn keyboard_interactivity(mut self, k: KeyboardInteractivity) -> Self {
        self.interactivity = k;
        self
    }

    /// Pin this surface to a specific output. Without a call to this method
    /// the compositor picks (usually whichever output the user's pointer is
    /// on).
    pub fn output(mut self, o: OutputId) -> Self {
        self.output = Some(o);
        self
    }

    pub fn build(self) -> crate::Result<SurfaceId> {
        crate::runtime::build_layer_surface(self)
    }
}
