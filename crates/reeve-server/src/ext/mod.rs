//! reeve server extensions (docs/build-charter.md CODE BOUNDARY):
//! whole modules behind cargo features, default ON; core never depends
//! on ext items. Each module wires itself into router.rs/render.rs
//! under its own `cfg(feature = "ext-<name>")` gates.

#[cfg(feature = "ext-channel")]
pub mod channel;
#[cfg(feature = "ext-federation")]
pub mod federation;
// C12 §10.4 /install bootstrap: named without the ext- prefix because
// spec/reeve/08-packaging.md §10.4 and the build charter name the
// feature `embedded-agents` verbatim; boundary rules are identical.
#[cfg(feature = "embedded-agents")]
pub mod install;
#[cfg(feature = "ext-logs")]
pub mod logs;
#[cfg(feature = "ext-rollouts")]
pub mod rollouts;
#[cfg(feature = "ext-secrets")]
pub mod secrets;
#[cfg(feature = "ext-sse")]
pub mod sse;
#[cfg(feature = "ext-terminal")]
pub mod terminal;
