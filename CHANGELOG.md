# Changelog

All notable changes to `darwinscope` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0]

Exports and imports are no longer read through goblin: this release
decodes the export trie and the legacy bind-opcode streams in-house, to
the same standard as the rest of the crate (bounded reads, no recursion
the input controls, fail-soft per row). Both decoders are checked
against goblin's on the fixtures, so the rows they state are the rows
they stated before.

### Added

- An in-house export-trie walker in `export`. It is iterative (an
  explicit stack, so no input-controlled recursion depth), visits each
  node offset once (an edge pointing back up the trie cannot loop it),
  reads only inside the trie's byte range and inside each node's stated
  terminal payload, and ends a single branch - not the walk - on a node
  it cannot read. `ExportInfo::Reexport` now also carries the
  `lib_ordinal` the trie node states.
- An in-house bind-opcode interpreter in `import`, exposed as
  `BindStream` over a `BindImage` of `BindSegment`s. It runs dyld's
  opcode state machine lazily, one row per bind as the consumer pulls
  it, and refuses what dyld refuses: a bind into a segment the image
  does not have or to a slot that does not fit inside its segment (this
  also bounds every repeat count), an opcode dyld does not know, and the
  threaded-bind opcode whose rows belong to a chain this interpreter
  does not walk.
- `ObjcRuntime::selector_refs()` yields the new `SelRef`, pairing each
  `__objc_selrefs` slot's address with the selector it names. The slot
  address is what a cross-reference index needs.
- `dylib::dylib_ordinals()` builds the image's dylib ordinal table: one
  install name per dependency load command, in load-command order, so
  entry `n - 1` is library ordinal `n` as dyld numbers it. A command
  whose name cannot be read keeps its slot, because dropping it would
  shift every later ordinal onto the wrong library.
- `fixup::resolve_lib_ordinal()` is now the one resolver for every
  encoding that names a library by ordinal - chained-fixup imports,
  legacy bind opcodes and export-trie re-exports - over `i64`, so the
  special self / main-executable / flat-namespace / weak ordinals and an
  out-of-range one answer the same way everywhere.
- `SwiftRuntime::has_mangled_name_pool()` reports `__swift5_typeref`,
  the member that was missing from the presence-accessor family.
- `VTableIter`, `OverrideEntryIter`, `DefaultOverrideEntryIter` and
  `PrespecializationIter` gained `new(rt, base_va, count)`.

### Changed

- **Breaking.** `ObjcRuntime::selector_refs()` yields `SelRef` instead
  of `&str`; the selector is `SelRef::selector`.
- **Breaking.** `ExportInfo::Reexport::lib` is `Option<&str>`: `None`
  when no load command holds the re-export's library ordinal, which the
  new `lib_ordinal` field states regardless.
- `MachoBinary::exports()` and `::imports()` return iterators bound to
  the image's data lifetime rather than to the `&self` borrow, so their
  rows can outlive the borrow that produced them.
- `exports()` follows dyld's precedence explicitly: the
  `LC_DYLD_EXPORTS_TRIE` trie when the image carries one, else the
  `LC_DYLD_INFO[_ONLY]` export trie.
- Restricted visibility is gone from the crate. `pub(crate)` and
  `pub(super)` are no longer used anywhere: an item is either
  module-private or `pub` and reachable from the crate root through a
  `pub mod` or an explicit re-export, and `unreachable_pub = "deny"`
  keeps it that way. Of the 204 restricted declarations, the ones used
  only inside their own module became private; the cross-module ones
  became public API, among them the section finders, the Obj-C list
  walkers and the Swift trailing-object decoders.

### Fixed

- Public items with no public path. The Obj-C `FAST_IS_SWIFT_LEGACY` /
  `FAST_IS_SWIFT_STABLE` / `FAST_HAS_DEFAULT_RR` constants and the Swift
  `ClassBody` / `StructBody` / `EnumBody` types were `pub` inside
  private modules, so no consumer could name them - `ClassBody` not even
  in the rendered docs, where `TypeKindBody::Class(ClassBody)` links to
  it.
- The README linked to a `ToDo.md` that is not part of the repository,
  the same dead-link class fixed for the crate docs in 0.1.1. Its
  feature table was also missing the `LC_DATA_IN_CODE`, compact-unwind
  and Blocks surfaces that 0.1.3 and 0.1.4 added.
- Doc comments describing a goblin lifetime collapse that no longer
  applies now that neither bind encoding goes through goblin.

## [0.1.4]

### Added

- `MachoBinary::unwind_info()` reads the compact unwind table in
  `__TEXT,__unwind_info` through the new `unwind` module. `UnwindInfo::functions()`
  lists every function's start address and compact unwind encoding, walking both
  regular and compressed second-level pages; `UnwindInfo::lsdas()` pairs each
  function that handles exceptions with its LSDA (its table in `__gcc_except_tab`);
  `UnwindInfo::end()` gives the address one past the last function. Function
  starts are a disassembler's entry points, and the LSDAs lead to the landing pads
  that are reached only by unwinding.

## [0.1.3]

### Added

- `MachoBinary::data_in_code()` walks `LC_DATA_IN_CODE`: each entry's offset,
  length and `DICE_KIND_*` value (`DataInCodeKind`, with the entry width of the
  jump-table kinds), resolved to the virtual address its segment maps it to. A
  disassembler reads it to keep jump tables and literal islands out of the
  instruction stream.

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

[0.2.0]: https://github.com/ATRAPSLLC/darwinscope/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/ATRAPSLLC/darwinscope/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/ATRAPSLLC/darwinscope/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/ATRAPSLLC/darwinscope/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/ATRAPSLLC/darwinscope/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ATRAPSLLC/darwinscope/releases/tag/v0.1.0
