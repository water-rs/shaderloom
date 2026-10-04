#![doc = include_str!("../README.md")]

/// Build-time WGSL compilation into backend-native embedded artifacts.
///
/// Enable `build` on the `[build-dependencies]` entry with
/// `default-features = false`: that side of the crate pulls `naga` only, so the
/// host graph of a cross-compile stays free of the `wgpu`/`wgpu-hal` stack.
#[cfg(feature = "build")]
pub mod build;

/// Compiles checked-in translated shader sources into platform binaries.
///
/// A consumer's `build.rs` calls these on Apple / Windows targets to
/// materialize `MetalLib` / DXIL under `OUT_DIR`. Always compiled: it needs
/// no `naga`, only `std` and the platform tool.
pub mod packaged;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::*;
