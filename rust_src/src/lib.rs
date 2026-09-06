//! C FFI bindings for Atuin shell history — in-process shell integration.
//!
//! This crate exposes a C-compatible API (`atuin_*`) that lets zsh and
//! PowerShell record and search Atuin history without spawning a subprocess.
//!
//! The heavy lifting lives in the upstream `atuin` library. This crate is only
//! the thin FFI boundary: it creates an `atuin::session::Session` (which
//! reuses the upstream history/search command code) and marshals C strings and
//! errors.

pub mod ffi;
