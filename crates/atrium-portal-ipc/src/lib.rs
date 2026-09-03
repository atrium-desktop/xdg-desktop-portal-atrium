//! Narrow runtime bridge from the Portal backend to compositor-owned Tessera
//! resources.
//!
//! The crate implements the Tessera IPC v29 wire contract independently and
//! deliberately exposes no compositor-internal types. The file chooser,
//! account consent, secrets, email, and other Portal-owned resources do not
//! belong on this boundary.

mod blob;
mod client;
mod codec;
mod schema;

#[cfg(feature = "test-server")]
pub mod testing;

pub use client::{Client, StreamFrame, StreamMessage, StreamPayload, StreamSlot, StreamStarted};
pub use schema::{
    AccentColor, ColorScheme, ConfirmPickResult, ConnectionCapabilities, Contrast,
    DesktopPreferences, Event, LOCAL_PORTAL_SCOPE, LeaseGrant, MIN_PROTOCOL_VERSION, OutputInfo,
    PROTOCOL_VERSION, PickKind, PickResult, Point, Rect, SettingsSnapshot, Size, StreamCursorMode,
    StreamPixelFormat, StreamTarget, WindowId,
};
