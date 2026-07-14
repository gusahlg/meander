//! Error and Result types for meander.

use thiserror::Error;

/// Anything that can go wrong inside meander.
#[derive(Debug, Error)]
pub enum Error {
    #[error("wayland: could not connect to the compositor: {0}")]
    Connect(#[from] wayland_client::ConnectError),

    #[error("wayland: dispatch failed: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),

    #[error("wayland: transport error: {0}")]
    Transport(#[from] wayland_client::backend::WaylandError),

    #[error(
        "wayland: required global '{0}' was not advertised by the compositor.\n\
         Meander needs wl_compositor, wl_shm and zwlr_layer_shell_v1; the latter \
         is advertised by river, sway, hyprland, wayfire and most wlroots-based \
         compositors."
    )]
    MissingGlobal(&'static str),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("rustix: {0}")]
    Rustix(#[from] rustix::io::Errno),

    #[error("surface id {0:?} not registered with this App")]
    NoSuchSurface(crate::SurfaceId),

    #[error("surface has not been configured yet — wait for Event::Configure before drawing")]
    NotConfigured,

    #[error(
        "both shm buffers are still held by the compositor — wait for the next \
         frame callback or buffer release before drawing again"
    )]
    BuffersBusy,

    #[error("layer surface was built pinned to output {0:?}, which is not known to this App")]
    UnknownOutput(crate::OutputId),

    #[error("font: {0}")]
    Font(&'static str),

    #[error("invalid buffer dimensions: {0}")]
    InvalidBufferDimensions(&'static str),

    #[error(
        "requested shm buffer allocation of {requested} bytes exceeds the \
         {max}-byte per-pool maximum"
    )]
    BufferTooLarge { requested: usize, max: usize },

    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),

    #[error(
        "compositor's {interface} is version {advertised}, but this feature \
         needs at least version {required}"
    )]
    UnsupportedProtocolVersion {
        interface: &'static str,
        advertised: u32,
        required: u32,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
