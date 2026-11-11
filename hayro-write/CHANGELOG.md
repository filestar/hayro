# Changelog

## [Unreleased]

### Added

- `extract_with` and `ExtractionOptions`: copy a page whole rather than as an `XObject`. It can keep the page's
  optional content (`/OC`), its trim, bleed and art boxes and `UserUnit`, and chosen document catalog entries such as
  `OCProperties` and `OutputIntents` (`ExtractionResult::catalog_entries`). `extract` behaves as before.

## [0.9.0] - 2026-11-02

This release has an [MSRV] of 1.92.

### Changed

- Breaking change: updated the re-exported `hayro-syntax` to 0.9. See its changelog for details.

### Removed

- Breaking change: the `logging` feature. ([#1648][] by [@LaurenzV][])

### Fixed

- Fix writing streams that use the `Crypt` filter. ([#1684][] by [@Vantrongs][])

[@LaurenzV]: https://github.com/LaurenzV
[@Vantrongs]: https://github.com/Vantrongs

[#1648]: https://github.com/LaurenzV/hayro/pull/1648
[#1684]: https://github.com/LaurenzV/hayro/pull/1684

[MSRV]: ../README.md#minimum-supported-rust-version-msrv

[Unreleased]: https://github.com/LaurenzV/hayro/compare/hayro-write-v0.9.0...HEAD
[0.9.0]: https://github.com/LaurenzV/hayro/compare/hayro-write-v0.8.0...hayro-write-v0.9.0
