# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/water-rs/shaderloom/releases/tag/v0.1.0) - 2026-08-26

### Added

- *(ffi)* [**breaking**] gate the GPU stack behind a default-on `gpu` feature
- *(filtrate)* give hosts a shared per-device shader module cache

### Fixed

- assert the value, not the emptiness predicate

### Other

- stand shaderloom up as its own repository
- *(shaderloom)* say why the README's examples are not compiled
- Give every unsafe a reason the compiler can check, workspace-wide
- upgrade workspace dependencies
- Add cross-platform shader AOT with Shaderloom
