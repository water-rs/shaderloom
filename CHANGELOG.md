# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/water-rs/shaderloom/compare/v0.1.2...v0.2.0) - 2026-10-04

### Added

- [**breaking**] upgrade to wgpu 30
- [**breaking**] split the build-time shader toolchain from the runtime
- bring the flame demonstration home

### Fixed

- *(release)* mark the flame example unpublished in release-plz ([#32](https://github.com/water-rs/shaderloom/pull/32))
- *(release)* keep the library's tags as v<version> ([#28](https://github.com/water-rs/shaderloom/pull/28))
- [**breaking**] enable only the runtime API by default ([#25](https://github.com/water-rs/shaderloom/pull/25))
- *(ci)* repair lint checks and trusted publishing
- *(example)* park the redraw chain while the window is occluded
- *(build)* pin the MSL pipeline options a passthrough artifact needs
- *(example)* keep the redraw chain alive and run the loop on Wait
- *(ci)* pin the toolchain without disarming the MSRV gate ([#18](https://github.com/water-rs/shaderloom/pull/18))

### Other

- let release-plz and hotfix branches into main ([#30](https://github.com/water-rs/shaderloom/pull/30))
- merge main back into dev after v0.1.2
- prebuilt cargo-outdated + incremental/debuginfo trims ([#17](https://github.com/water-rs/shaderloom/pull/17))
- run tests with cargo nextest ([#16](https://github.com/water-rs/shaderloom/pull/16))
- provision Linux sysdeps and dxc for the workspace check
- publish to crates.io via OIDC trusted publishing ([#11](https://github.com/water-rs/shaderloom/pull/11))
- gate pull requests into main so only dev may merge ([#12](https://github.com/water-rs/shaderloom/pull/12))

## [0.1.2](https://github.com/water-rs/shaderloom/compare/v0.1.1...v0.1.2) - 2026-09-02

### Fixed

- compile SPIR-V with wgpu's Vulkan writer options

### Other

- Merge pull request #10 from water-rs/dev
- add rust-cache to dep-check workflow

## [0.1.1](https://github.com/water-rs/shaderloom/compare/v0.1.0...v0.1.1) - 2026-08-27

### Other

- ship the licence texts, and release 0.1.1
- add weekly dependency check workflow
- lint on stable
