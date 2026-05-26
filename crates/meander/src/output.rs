//! Output (monitor) enumeration.

use wayland_client::protocol::wl_output;

/// Opaque handle to a monitor. Stable within a single `App` lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub(crate) u32);

impl OutputId {
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Public, read-only snapshot of what we know about a monitor.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub id: OutputId,
    /// xdg-output-style name, or `None` if the compositor only exposes
    /// wl_output. River advertises names ("HDMI-A-1", etc.) on wl_output v4.
    pub name: Option<String>,
    pub description: Option<String>,
    /// Physical size in millimetres reported by the compositor (`(0, 0)` when
    /// unknown, which is common on Wayland).
    pub physical_size_mm: (i32, i32),
    /// Position of the output in the global compositor space.
    pub position: (i32, i32),
    /// Current mode, in pixels: `(width, height, refresh_mHz)`.
    pub mode: Option<(i32, i32, i32)>,
    pub scale: i32,
    pub transform: Transform,
    pub subpixel: Subpixel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    Normal,
    Rotated90,
    Rotated180,
    Rotated270,
    Flipped,
    FlippedRotated90,
    FlippedRotated180,
    FlippedRotated270,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subpixel {
    Unknown,
    None,
    HorizontalRgb,
    HorizontalBgr,
    VerticalRgb,
    VerticalBgr,
}

pub(crate) struct OutputEntry {
    pub(crate) info: OutputInfo,
    pub(crate) output: wl_output::WlOutput,
    /// wl_registry's `name`, used to clean up on `global_remove`.
    pub(crate) registry_name: u32,
}

impl OutputEntry {
    pub(crate) fn new(output: wl_output::WlOutput, id: OutputId, registry_name: u32) -> Self {
        Self {
            info: OutputInfo {
                id,
                name: None,
                description: None,
                physical_size_mm: (0, 0),
                position: (0, 0),
                mode: None,
                scale: 1,
                transform: Transform::Normal,
                subpixel: Subpixel::Unknown,
            },
            output,
            registry_name,
        }
    }
}

pub(crate) fn map_transform(t: wl_output::Transform) -> Transform {
    use wl_output::Transform as T;
    match t {
        T::Normal => Transform::Normal,
        T::_90 => Transform::Rotated90,
        T::_180 => Transform::Rotated180,
        T::_270 => Transform::Rotated270,
        T::Flipped => Transform::Flipped,
        T::Flipped90 => Transform::FlippedRotated90,
        T::Flipped180 => Transform::FlippedRotated180,
        T::Flipped270 => Transform::FlippedRotated270,
        _ => Transform::Normal,
    }
}

pub(crate) fn map_subpixel(s: wl_output::Subpixel) -> Subpixel {
    use wl_output::Subpixel as S;
    match s {
        S::Unknown => Subpixel::Unknown,
        S::None => Subpixel::None,
        S::HorizontalRgb => Subpixel::HorizontalRgb,
        S::HorizontalBgr => Subpixel::HorizontalBgr,
        S::VerticalRgb => Subpixel::VerticalRgb,
        S::VerticalBgr => Subpixel::VerticalBgr,
        _ => Subpixel::Unknown,
    }
}
