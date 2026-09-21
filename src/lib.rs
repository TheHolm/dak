//! Library crate of DAK (Dynamic Ajazz Keyboard), exposing config loading/validation,
//! scene/action logic, device addressing (baseplane), filterable output, text
//! rendering, and hardware detection for the keypad controller.

// FreeBSD has no HID backend in the real, published `mirajazz`/`async-hid` crates
// (see AGENTS.md); `vendor/` carries a small FreeBSD-only fork, kept out of every
// other platform's dependency graph via Cargo.toml's target-specific tables (Linux
// and everything else still resolve the genuine crates.io releases, untouched).
// Cargo requires one dependency name to resolve to a single source kind (registry vs
// path) across all targets, so the FreeBSD path dependencies are declared under
// different manifest keys (`mirajazz-freebsd`/`async-hid-freebsd`); these two lines
// re-export them under the plain names the rest of this crate uses unconditionally,
// so no other source file needs a platform-specific branch.
#[cfg(target_os = "freebsd")]
extern crate async_hid_freebsd as async_hid;
#[cfg(target_os = "freebsd")]
extern crate mirajazz_freebsd as mirajazz;

pub mod actions;
pub mod baseplane;
pub mod cli;
pub mod hardware;
pub mod log;
pub mod map;
pub mod press;
pub mod text;
pub mod variables;
