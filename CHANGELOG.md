# Changelog

All notable changes to `darwinscope` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.2]

### Added

- `Section::holds_instructions()` reports whether a section's own attributes
  (`S_ATTR_PURE_INSTRUCTIONS` / `S_ATTR_SOME_INSTRUCTIONS`) mark its bytes as machine
  code. The answer comes from the section itself, not from its section type or its
  segment's `initprot`, so `__text` and `__cstring` are correctly told apart inside
  the same `r-x` `__TEXT` segment.
- `deny.toml` and a `cargo deny check bans` CI job. It bans `arrayref >= 0.3.10`,
  which was published by a compromised crates.io account.

### Changed

- Bumped bitflags to 2.13.2, plist to 1.10.1 and blake3 to 1.8.7. blake3 1.8.7 no
  longer depends on `arrayref`, so that crate has left the dependency graph entirely.
- Replaced em-dashes throughout documentation, comments and the package description
  with plain hyphens.

### Fixed

- Several doc comments had a line beginning with a dash, which rustdoc rendered as a
  stray bullet list in the middle of a sentence.

## [0.1.1]

### Added

- `DerEntitlements::pairs()` decodes the `fade7172` DER-entitlements container into
  typed key/value pairs via the new `DerEntitlementValue` enum, modelling the plist
  types Apple emits (bool, integer, string, nested array) and preserving anything
  else as `Other` with its raw DER tag.
- `TypeDescriptor` resolution of Swift stored-property byte offsets from the static
  field-offset vector, for types whose metadata is statically present in the file.

### Fixed

- Dead documentation links. The crate docs linked to `RESEARCH.md` and `ToDo.md`,
  which are not part of the repository, so every such link in the published rustdoc
  was a 404.
- Integration tests inherited the package's panic-safety lint denials with no
  escape hatch - the `cfg_attr(test, allow(...))` in `src/lib.rs` covers only the
  library crate, not the separate crates under `tests/`.
- `shannon_entropy` no longer suppresses `clippy::indexing_slicing` /
  `arithmetic_side_effects`; the histogram is indexed through `get_mut` and
  accumulated with `saturating_add`, so the lints pass on their own merits.

### Changed

- Panic-safety lints are declared in `Cargo.toml` under `[lints]`, so they enforce
  on every build regardless of the consuming workspace.
- CI lints `--all-targets --all-features`, so lint failures outside the library are
  gated rather than invisible.
- Recorded ATRAPS LLC as copyright holder and added a `NOTICE` file.
- Dropped the deprecated `authors` field and repointed `repository` at the organisation.
- Bumped goblin to 0.10.7 and bitflags to 2.13.0.
- Publishing now uses crates.io trusted publishing instead of a stored registry token.

## [0.1.0]

Initial release.

[0.1.2]: https://github.com/ATRAPSLLC/darwinscope/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/ATRAPSLLC/darwinscope/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ATRAPSLLC/darwinscope/releases/tag/v0.1.0
