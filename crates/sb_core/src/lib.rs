//! scrollback's headless domain logic (docs/PLAN.md §3): the archive and its catalogue,
//! locations and their transports, the sync engine, providers, the transcript model, filing and
//! search.
//!
//! gpui: **never**, not even transitively (`script/check` step 3), so a headless sync can be
//! built on this crate later (PLAN D2).
//!
//! Agent-agnostic (PLAN D8): everything specific to a coding agent lives in its provider, under
//! [`provider`]. No other module names an agent (`script/check` step 4).
//!
//! The modules are the homes PLAN §3 names. Syncing a location into the archive is built
//! ([`archive`], [`catalogue`], [`transport`], [`provider`], [`sync`]); [`transcript`],
//! [`filing`] and [`search`] are empty until the phases that build them (PLAN §13).

pub mod archive;
pub mod catalogue;
pub mod filing;
pub mod hash;
pub mod location;
pub mod provider;
pub mod search;
pub mod sync;
pub mod transcript;
pub mod transport;

pub use toolbox_process::CancelToken;
