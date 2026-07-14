//! Negotiated Wayland protocol capabilities.
//!
//! The registry binds each global at `min(advertised, meander's max)`. Later
//! code must not assume a request or event exists just because meander knows
//! about it — the *negotiated* version decides. This module records those
//! versions and answers the per-feature "is this request/event available?"
//! questions as pure functions, so the dispatch decisions are unit-testable
//! without a live compositor.
//!
//! # Minimum and maximum supported versions
//!
//! | global                  | min | max | notes                                   |
//! |-------------------------|-----|-----|-----------------------------------------|
//! | `wl_compositor`         | 1   | 4   | v3 `set_buffer_scale`, v4 `damage_buffer` |
//! | `wl_shm`                | 1   | 1   | formats advertised via `format` events  |
//! | `zwlr_layer_shell_v1`   | 1   | 4   | v4 adds `on_demand` keyboard mode        |
//! | `wl_seat` / `wl_pointer`| 1   | 7   | v3 `release`, v5 `frame`/axis extras, v8 `value120` |

use crate::surface::KeyboardInteractivity;

pub(crate) const MAX_COMPOSITOR_VERSION: u32 = 4;
pub(crate) const MAX_SHM_VERSION: u32 = 1;
pub(crate) const MAX_LAYER_SHELL_VERSION: u32 = 4;
pub(crate) const MAX_SEAT_VERSION: u32 = 7;

/// Versions negotiated for the singleton globals.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Capabilities {
    pub(crate) compositor_version: u32,
    pub(crate) shm_version: u32,
    pub(crate) layer_shell_version: u32,
    /// Whether the compositor advertised the swap-free `Abgr8888` shm format.
    pub(crate) abgr_supported: bool,
}

impl Capabilities {
    /// `set_buffer_scale` — `wl_surface` request added in `wl_compositor` v3.
    pub(crate) fn supports_set_buffer_scale(&self) -> bool {
        self.compositor_version >= 3
    }

    /// `damage_buffer` — buffer-coordinate damage added in `wl_compositor` v4.
    /// Older servers only have surface-local `damage` (logical coordinates).
    pub(crate) fn supports_damage_buffer(&self) -> bool {
        self.compositor_version >= 4
    }

    /// Whether a requested keyboard-interactivity mode is expressible on the
    /// negotiated layer-shell version. `on_demand` was added in v4; `none` and
    /// `exclusive` exist from v1.
    pub(crate) fn supports_keyboard_mode(&self, mode: KeyboardInteractivity) -> bool {
        match mode {
            KeyboardInteractivity::None | KeyboardInteractivity::Exclusive => {
                self.layer_shell_version >= 1
            }
            KeyboardInteractivity::OnDemand => self.layer_shell_version >= 4,
        }
    }
}

/// Version-derived capabilities of one bound `wl_pointer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PointerCaps {
    pub(crate) version: u32,
}

impl PointerCaps {
    pub(crate) fn new(version: u32) -> Self {
        Self { version }
    }

    /// `wl_pointer.frame` groups a batch of events. Added in v5. On older
    /// versions each event must be delivered immediately.
    pub(crate) fn has_frame(&self) -> bool {
        self.version >= 5
    }

    /// `wl_pointer.release` (client destructor) added in v3. On v1/v2 the
    /// object must simply be dropped, never explicitly released.
    pub(crate) fn has_release(&self) -> bool {
        self.version >= 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(comp: u32, shm: u32, ls: u32) -> Capabilities {
        Capabilities {
            compositor_version: comp,
            shm_version: shm,
            layer_shell_version: ls,
            abgr_supported: false,
        }
    }

    #[test]
    fn set_buffer_scale_requires_v3() {
        assert!(!caps(2, 1, 4).supports_set_buffer_scale());
        assert!(caps(3, 1, 4).supports_set_buffer_scale());
        assert!(caps(4, 1, 4).supports_set_buffer_scale());
    }

    #[test]
    fn damage_buffer_requires_v4() {
        assert!(!caps(3, 1, 4).supports_damage_buffer());
        assert!(caps(4, 1, 4).supports_damage_buffer());
    }

    #[test]
    fn on_demand_keyboard_requires_layer_shell_v4() {
        let c = caps(4, 1, 3);
        assert!(c.supports_keyboard_mode(KeyboardInteractivity::None));
        assert!(c.supports_keyboard_mode(KeyboardInteractivity::Exclusive));
        assert!(!c.supports_keyboard_mode(KeyboardInteractivity::OnDemand));
        assert!(caps(4, 1, 4).supports_keyboard_mode(KeyboardInteractivity::OnDemand));
    }

    #[test]
    fn pointer_frame_matrix() {
        assert!(!PointerCaps::new(4).has_frame());
        assert!(PointerCaps::new(5).has_frame());
        assert!(PointerCaps::new(7).has_frame());
    }

    #[test]
    fn pointer_release_matrix() {
        assert!(!PointerCaps::new(2).has_release());
        assert!(PointerCaps::new(3).has_release());
    }
}
