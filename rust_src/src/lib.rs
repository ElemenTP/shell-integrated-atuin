//! C FFI bindings for Atuin shell history — in-process shell integration.
//!
//! This crate exposes a C-compatible API (`atuin_*`) that lets zsh and
//! PowerShell record and search Atuin history without spawning a subprocess.
//!
//! # Safety
//!
//! All FFI functions wrap their bodies in `catch_unwind` so a Rust panic never
//! unwinds across the C ABI boundary. Errors are reported through return codes
//! and a global mutex-guarded error string (`atuin_last_error`). A `creator_pid`
//! fork guard prevents calls from zsh's forked children (`$()`, `&`, pipelines)
//! from touching the tokio runtime in its post-fork corrupted state.

mod tui;
mod tui_input;

pub mod ffi;
