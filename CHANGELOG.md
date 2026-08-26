# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/water-rs/shaderloom/releases/tag/v0.1.0) - 2026-08-26

First release as a standalone crate. The code previously lived inside the
WaterUI repository; its history is preserved here.

### Added

- Compile WGSL during a package build and embed the result, so native backends
  load a shader module without translating WGSL at runtime.
- Emit each backend's own artifact: `MetalLib` for Metal, DXIL for Direct3D 12,
  SPIR-V for Vulkan. GLES and browser WebGPU keep validated WGSL, because
  `wgpu` exposes no portable offline binary input for them; they still gain
  build-time parsing and validation.
- Expose a shared per-device shader module cache, so hosts compiling the same
  module for several surfaces pay for it once.
- Keep the runtime dependency to `wgpu` alone. `naga` enters only through the
  optional `build` feature, so a consumer does not ship a shader compiler.
