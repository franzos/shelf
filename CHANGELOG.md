# Changelog

## [0.1.2] - 2026-05-24

### Added
- `--all` flag on `plan` and `run` to show every action and health entry.
- Phase spinner on stderr during long runs.

### Changed
- `plan` and `run` suppress `skip-duplicate`, `skip-conflict`, and
  `missing-date` by default; counts still appear in the summary.

### Fixed
- Missing `package.description` broke `cargo generate-rpm`.

## [0.1.1] - 2026-05-12

### Added
- `.deb` and `.rpm` packaging metadata for release builds.
