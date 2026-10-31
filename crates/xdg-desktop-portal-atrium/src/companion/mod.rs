//! The portal's out-of-process companion calls.
//!
//! ADR-0026 distributed every interactive prompt to the component that owns
//! it. Most of those are compositor ops reached directly over
//! [`atrium_portal_ipc`] (see [`crate::ipc`]); this module holds the ones that
//! are a *process boundary*, and nothing else belongs here. Today that is
//! exactly one:
//!
//! - [`arca`]: the FileChooser dialog, spoken as `arca --chooser-prompt` over
//!   stdin/stdout under the contract in [`atrium_arca_contract`] (ADR-0027).

pub(crate) mod arca;
