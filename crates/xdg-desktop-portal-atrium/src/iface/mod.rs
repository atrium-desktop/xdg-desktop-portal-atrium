//! One module per served `org.freedesktop.impl.portal.*` interface.
//!
//! ADR-0025 §3: a portal interface is D-Bus projection plus that interface's
//! own request policy, and no two interfaces share either. Keeping them in one
//! directory makes the rule visible and reviewable — an interface module that
//! starts importing a sibling is the signal that something belongs in a
//! shared crate or in [`crate::companion`], not a signal to add a
//! cross-import.
//!
//! What does **not** live here:
//!
//! - [`crate::ipc`]: the compositor client several interfaces share.
//! - [`crate::companion`]: the one out-of-process call (arca).
//! - [`crate::screencast`]: ScreenCast plus the session registry and restore
//!   store that exist only for it.
//! - [`crate::fsx`] and [`crate::uri`]: filesystem and URI helpers whose
//!   consumers span interfaces.

pub(crate) mod access;
pub(crate) mod account;
pub(crate) mod app_chooser;
pub(crate) mod background;
pub(crate) mod email;
pub(crate) mod file_chooser;
pub(crate) mod inhibit;
pub(crate) mod lockdown;
pub(crate) mod notification;
pub(crate) mod open_uri;
pub(crate) mod print;
pub(crate) mod screenshot;
pub(crate) mod settings;
pub(crate) mod wallpaper;
