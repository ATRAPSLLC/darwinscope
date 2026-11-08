//! Exports - symbols this image makes available to other dylibs.
//!
//! Walks the export trie dyld reads: the standalone trie of
//! `LC_DYLD_EXPORTS_TRIE` when the image carries one, else the
//! `export_off` / `export_size` pair of `LC_DYLD_INFO[_ONLY]` (dyld
//! prefers the former the same way).
//!
//! ## The walk
//!
//! A trie node is a ULEB128 terminal size, the terminal payload
//! (flags, then the address, the re-export ordinal and name, or the
//! stub and resolver offsets), a one-byte child count and, per child,
//! a NUL-terminated edge label and the child's ULEB128 offset from the
//! trie start (cite: `dyld/src/ImageLoaderMachOCompressed.cpp`
//! `trieWalk`; `ld64/src/other/dyldinfo.cpp` `parseTrie`).
//!
//! Every input byte is adversarial, so the walker is iterative (an
//! explicit stack: no recursion depth the input controls), visits each
//! node offset once (an edge pointing back up the trie cannot loop
//! it), reads only inside the trie's own byte range and inside each
//! node's stated terminal payload, and resolves a re-export's library
//! ordinal through the image's dylib table instead of indexing it. A
//! node the walker cannot read ends that branch only: the image's
//! other exports are still stated.
//!
//! Names are reconstructed from trie edges and therefore *owned*
//! `String`s rather than borrowed slices - the trie does not preserve
//! a contiguous range of bytes for each export name.

use std::collections::HashSet;

use goblin::mach::exports::{
    EXPORT_SYMBOL_FLAGS_KIND_MASK, EXPORT_SYMBOL_FLAGS_REEXPORT,
    EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER, EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION,
};

use crate::{
    fixup::resolve_lib_ordinal,
    util::{read_cstr_at, read_uleb128},
};

/// View over a single exported symbol resolved from the export trie.
#[derive(Debug)]
pub struct Export<'p> {
    /// Mangled symbol name (as `dyld` matches it).
    pub name: String,
    /// Symbol kind - regular, absolute, thread-local, or unknown.
    pub kind: ExportKind,
    /// Raw `EXPORT_SYMBOL_FLAGS_*` value from the trie node.
    pub flags: u64,
    /// Whether `EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION` is set.
    pub is_weak_definition: bool,
    /// Detailed payload depending on the kind / flags.
    pub info: ExportInfo<'p>,
    /// Offset / address recorded for this export - the address for
    /// `Regular`, the image-relative stub offset for `Stub`, `0` for
    /// `Reexport`.
    pub offset: u64,
}

/// Trie-node kind extracted from the low two bits of the flags
/// (`EXPORT_SYMBOL_FLAGS_KIND_MASK = 0x03`).
///
/// Cite: `mach-o/loader.h:1494-1503` (`EXPORT_SYMBOL_FLAGS_KIND_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    /// `EXPORT_SYMBOL_FLAGS_KIND_REGULAR = 0x00` - an ordinary
    /// function or data symbol whose body lives at a VM address in
    /// this image. The vast majority of trie entries are this kind.
    Regular,
    /// `EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE = 0x02` - an absolute
    /// constant (e.g. `mh_execute_header`'s sentinel value); the
    /// "address" is a literal symbol value, not a VM address.
    Absolute,
    /// `EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL = 0x01` - a TLS
    /// variable; the address is a thread-local offset, accessed via
    /// `__thread_vars` on launch.
    ThreadLocal,
    /// Unknown kind value - preserved for round-trip. Reserved for
    /// future `EXPORT_SYMBOL_FLAGS_KIND_*` constants.
    Other(u64),
}

impl ExportKind {
    /// Reads the kind from a trie node's flags.
    fn from_flags(flags: u64) -> Self {
        match flags & EXPORT_SYMBOL_FLAGS_KIND_MASK {
            0x00 => Self::Regular,
            0x01 => Self::ThreadLocal,
            0x02 => Self::Absolute,
            other => Self::Other(other),
        }
    }
}

/// Per-kind / per-flag payload of an [`Export`].
///
/// Carries the *additional* fields that depend on which trie-node
/// flavor this export uses. The [`ExportKind`] tag and `flags` value
/// already cover the common case; this enum only fires on re-exports
/// and stub-and-resolver exports.
#[derive(Debug)]
pub enum ExportInfo<'p> {
    /// Regular export: lives at `address` (VM address). The
    /// overwhelming majority of trie entries fall into this variant.
    Regular {
        /// VM address of the export's body in *this* image.
        address: u64,
    },
    /// Re-export with `EXPORT_SYMBOL_FLAGS_REEXPORT` set in the trie
    /// node - this image declares the symbol but forwards resolution
    /// to another dylib. dyld follows the chain at load time.
    Reexport {
        /// Install-name of the dylib the re-export forwards into,
        /// resolved from `lib_ordinal` through the image's
        /// `LC_LOAD_*_DYLIB` table. `None` when no load command holds
        /// that ordinal. Zero-copy reborrow.
        lib: Option<&'p str>,
        /// The library ordinal the trie node states: 1-based into the
        /// image's `LC_LOAD_*_DYLIB` commands in load-command order.
        lib_ordinal: u64,
        /// Symbol name in the target dylib. `None` means "use this
        /// entry's own trie name", which the toolchain emits when the
        /// re-exported name matches the original - saves bytes in the
        /// trie.
        lib_symbol_name: Option<&'p str>,
    },
    /// Stub-and-resolver export
    /// (`EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER`). Used by dispatch
    /// trampolines and `__attribute__((ifunc))`-style entries: the
    /// `stub_offset` is the indirect call target dyld writes into
    /// non-lazy pointers, while `resolver_offset` is invoked once per
    /// dylib load to compute the real target. This is how
    /// `libsystem_pthread.dylib` exposes per-CPU optimised builtins.
    Stub {
        /// Image-relative offset of the stub trampoline.
        stub_offset: u64,
        /// Image-relative offset of the resolver function dyld calls
        /// once.
        resolver_offset: u64,
    },
}

/// Iterator over [`Export`] rows, in trie pre-order (a node before its
/// children, children in edge order).
///
/// Empty when the binary has no `LC_DYLD_EXPORTS_TRIE` /
/// `LC_DYLD_INFO[_ONLY]` export trie, when the trie's byte range lies
/// outside the image, or when the binary genuinely exports nothing.
pub struct ExportIter<'p> {
    /// The decoded rows, in trie pre-order.
    items: std::vec::IntoIter<Export<'p>>,
}

impl<'p> ExportIter<'p> {
    /// Walks the trie at `range` of `data`, resolving re-export
    /// ordinals through `dylibs`, the image's dylib ordinal table.
    pub fn new(data: &'p [u8], range: Option<core::ops::Range<usize>>, dylibs: &[&'p str]) -> Self {
        let items = range
            .and_then(|range| data.get(range))
            .map(|trie| walk(trie, dylibs))
            .unwrap_or_default();
        Self {
            items: items.into_iter(),
        }
    }
}

impl<'p> Iterator for ExportIter<'p> {
    type Item = Export<'p>;
    fn next(&mut self) -> Option<Self::Item> {
        self.items.next()
    }
}

/// Walks every node of `trie` once, depth first, and returns the
/// exports its terminal nodes state.
fn walk<'p>(trie: &'p [u8], dylibs: &[&'p str]) -> Vec<Export<'p>> {
    let mut exports = Vec::new();
    let mut visited: HashSet<usize> = HashSet::new();
    let mut stack: Vec<(usize, String)> = vec![(0, String::new())];
    while let Some((node, name)) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        let Some((terminal, children)) = node_parts(trie, node) else {
            continue;
        };
        let branches = read_children(trie, children);
        for &(label, child) in branches.iter().rev() {
            if !visited.contains(&child) {
                stack.push((child, format!("{name}{label}")));
            }
        }
        if let Some(export) = terminal.and_then(|payload| decode_terminal(payload, name, dylibs)) {
            exports.push(export);
        }
    }
    exports
}

/// Splits the node at `node` into its terminal payload (`None` for a
/// node that states no export) and the offset of its child list.
/// `None` when the node's header or payload lies outside `trie`.
fn node_parts(trie: &[u8], node: usize) -> Option<(Option<&[u8]>, usize)> {
    let (terminal_size, header) = read_uleb128(trie.get(node..)?)?;
    let payload_start = node.checked_add(header)?;
    let payload_end = payload_start.checked_add(usize::try_from(terminal_size).ok()?)?;
    let payload = trie.get(payload_start..payload_end)?;
    Some(((terminal_size != 0).then_some(payload), payload_end))
}

/// Reads the child list at `at`: a one-byte count, then per child a
/// NUL-terminated edge label and the child's ULEB128 node offset.
///
/// Stops at the first child it cannot read or whose offset lies
/// outside `trie`; the children before it are kept.
fn read_children(trie: &[u8], at: usize) -> Vec<(&str, usize)> {
    let mut branches = Vec::new();
    let Some(&count) = trie.get(at) else {
        return branches;
    };
    let mut offset = at.saturating_add(1);
    for _ in 0..count {
        let Some(label) = read_cstr_at(trie, offset) else {
            break;
        };
        let Some(after_label) = offset
            .checked_add(label.len())
            .and_then(|end| end.checked_add(1))
        else {
            break;
        };
        let Some((child, size)) = trie.get(after_label..).and_then(read_uleb128) else {
            break;
        };
        let Some(child) = usize::try_from(child)
            .ok()
            .filter(|&child| child < trie.len())
        else {
            break;
        };
        let Some(next) = after_label.checked_add(size) else {
            break;
        };
        branches.push((label, child));
        offset = next;
    }
    branches
}

/// Decodes one terminal payload into the export it states for `name`.
///
/// Flag precedence follows dyld: `EXPORT_SYMBOL_FLAGS_REEXPORT` on any
/// kind, then `EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER` on a regular
/// export, else a plain address. `None` when a field runs past the
/// payload.
fn decode_terminal<'p>(payload: &'p [u8], name: String, dylibs: &[&'p str]) -> Option<Export<'p>> {
    let (flags, mut at) = read_uleb128(payload)?;
    let kind = ExportKind::from_flags(flags);
    let mut uleb = || -> Option<u64> {
        let (value, size) = read_uleb128(payload.get(at..)?)?;
        at = at.checked_add(size)?;
        Some(value)
    };
    let (info, offset) = if flags & EXPORT_SYMBOL_FLAGS_REEXPORT != 0 {
        let lib_ordinal = uleb()?;
        let symbol = read_cstr_at(payload, at)?;
        let lib = i64::try_from(lib_ordinal)
            .ok()
            .map(|ordinal| resolve_lib_ordinal(ordinal, dylibs))
            .filter(|lib| !lib.is_empty());
        let info = ExportInfo::Reexport {
            lib,
            lib_ordinal,
            lib_symbol_name: (!symbol.is_empty()).then_some(symbol),
        };
        (info, 0)
    } else if kind == ExportKind::Regular && flags & EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER != 0 {
        let stub_offset = uleb()?;
        let resolver_offset = uleb()?;
        let info = ExportInfo::Stub {
            stub_offset,
            resolver_offset,
        };
        (info, stub_offset)
    } else {
        let address = uleb()?;
        (ExportInfo::Regular { address }, address)
    };
    Some(Export {
        name,
        kind,
        flags,
        is_weak_definition: flags & EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION != 0,
        info,
        offset,
    })
}

/// Re-export goblin's `EXPORT_SYMBOL_FLAGS_*` for callers that want
/// to inspect the raw flag value.
pub mod flags {
    pub use goblin::mach::exports::{
        EXPORT_SYMBOL_FLAGS_KIND_MASK, EXPORT_SYMBOL_FLAGS_REEXPORT,
        EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER, EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION,
    };
}

#[cfg(test)]
mod tests {
    use goblin::mach::{exports::ExportTrie, load_command::DyldInfoCommand};

    use super::*;

    /// goblin's own well-formed test trie (`goblin::mach::exports`
    /// tests).
    const GOBLIN_TRIE: [u8; 64] = [
        0x00, 0x01, 0x5f, 0x00, 0x05, 0x00, 0x02, 0x5f, 0x6d, 0x68, 0x5f, 0x65, 0x78, 0x65, 0x63,
        0x75, 0x74, 0x65, 0x5f, 0x68, 0x65, 0x61, 0x64, 0x65, 0x72, 0x00, 0x1f, 0x6d, 0x61, 0x00,
        0x23, 0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x78, 0x69, 0x6d, 0x75, 0x6d, 0x00, 0x30, 0x69,
        0x6e, 0x00, 0x35, 0x03, 0x00, 0xc0, 0x1e, 0x00, 0x03, 0x00, 0xd0, 0x1e, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    /// `(name, address)` of every regular export the walker states.
    fn regular(exports: &[Export<'_>]) -> Vec<(String, u64)> {
        exports
            .iter()
            .filter_map(|e| match e.info {
                ExportInfo::Regular { address } => Some((e.name.clone(), address)),
                _ => None,
            })
            .collect()
    }

    /// A root with one child edge `label` to a terminal node holding
    /// `payload`, the terminal node's own child list being `children`.
    fn one_child(label: &[u8], payload: &[u8], children: &[u8]) -> Vec<u8> {
        let mut trie = vec![0x00, 0x01];
        trie.extend_from_slice(label);
        trie.push(0);
        let child = trie.len() + 1;
        trie.push(child as u8);
        trie.push(payload.len() as u8);
        trie.extend_from_slice(payload);
        trie.extend_from_slice(children);
        trie
    }

    #[test]
    fn a_well_formed_trie_states_what_goblin_states() {
        let command = DyldInfoCommand {
            export_size: GOBLIN_TRIE.len() as u32,
            ..Default::default()
        };
        let expected: Vec<(String, u64)> = ExportTrie::new(&GOBLIN_TRIE, &command)
            .exports(&[])
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.offset))
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(regular(&walk(&GOBLIN_TRIE, &[])), expected);
    }

    #[test]
    fn a_child_edge_back_to_the_root_ends_the_walk() {
        // `_a` at 0x10, whose node lists the root as its child.
        let trie = one_child(b"_a", &[0x00, 0x10], &[0x01, b'x', 0x00, 0x00]);
        assert_eq!(regular(&walk(&trie, &[])), [("_a".to_string(), 0x10)]);
    }

    #[test]
    fn a_deep_trie_walks_without_recursion() {
        // A chain of 100,000 one-letter edges; only the last node is
        // terminal. Recursing per node would overflow the stack.
        let depth = 100_000usize;
        let mut trie = Vec::new();
        let mut offsets = Vec::new();
        for _ in 0..depth {
            offsets.push(trie.len());
            // terminal size 0, one child, label "a", child offset
            // (patched below as a 3-byte ULEB128).
            trie.extend_from_slice(&[0x00, 0x01, b'a', 0x00, 0x80, 0x80, 0x00]);
        }
        let leaf = trie.len();
        trie.extend_from_slice(&[0x02, 0x00, 0x2a, 0x00]);
        for (i, &at) in offsets.iter().enumerate() {
            let next = offsets.get(i + 1).copied().unwrap_or(leaf) as u32;
            trie[at + 4] = 0x80 | (next & 0x7f) as u8;
            trie[at + 5] = 0x80 | ((next >> 7) & 0x7f) as u8;
            trie[at + 6] = ((next >> 14) & 0x7f) as u8;
        }
        let exports = walk(&trie, &[]);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name.len(), depth);
    }

    #[test]
    fn a_reexport_from_an_unnamed_library_states_its_ordinal() {
        // `_r` re-exported from library ordinal 5 under `_s`; the image
        // loads one dylib.
        let trie = one_child(b"_r", &[0x08, 0x05, b'_', b's', 0x00], &[0x00]);
        let exports = walk(&trie, &["/usr/lib/libSystem.B.dylib"]);
        assert!(matches!(
            exports[0].info,
            ExportInfo::Reexport {
                lib: None,
                lib_ordinal: 5,
                lib_symbol_name: Some("_s")
            }
        ));
    }

    #[test]
    fn a_reexport_resolves_its_ordinal_through_the_dylib_table() {
        let trie = one_child(b"_r", &[0x08, 0x02, 0x00], &[0x00]);
        let exports = walk(&trie, &["/usr/lib/a.dylib", "/usr/lib/b.dylib"]);
        assert!(matches!(
            exports[0].info,
            ExportInfo::Reexport {
                lib: Some("/usr/lib/b.dylib"),
                lib_ordinal: 2,
                lib_symbol_name: None
            }
        ));
        assert_eq!(exports[0].offset, 0);
    }

    #[test]
    fn a_stub_and_resolver_export_states_both_offsets() {
        let trie = one_child(b"_f", &[0x10, 0x20, 0x30], &[0x00]);
        let exports = walk(&trie, &[]);
        assert!(matches!(
            exports[0].info,
            ExportInfo::Stub {
                stub_offset: 0x20,
                resolver_offset: 0x30
            }
        ));
        assert_eq!(exports[0].offset, 0x20);
    }

    #[test]
    fn an_unreadable_node_ends_only_its_branch() {
        // Root: two children. `_bad` points at a node whose terminal
        // size runs past the trie; `_ok` at a readable one.
        let mut trie = vec![0x00, 0x02];
        trie.extend_from_slice(b"_bad\0");
        let bad_at = trie.len();
        trie.push(0);
        trie.extend_from_slice(b"_ok\0");
        let ok_at = trie.len();
        trie.push(0);
        let bad = trie.len();
        trie.extend_from_slice(&[0x7f, 0x00]);
        let ok = trie.len();
        trie.extend_from_slice(&[0x02, 0x00, 0x44, 0x00]);
        trie[bad_at] = bad as u8;
        trie[ok_at] = ok as u8;
        assert_eq!(regular(&walk(&trie, &[])), [("_ok".to_string(), 0x44)]);
    }

    #[test]
    fn a_child_offset_past_the_trie_is_not_followed() {
        let trie = vec![0x00, 0x01, b'_', b'x', 0x00, 0x7f];
        assert!(walk(&trie, &[]).is_empty());
    }

    #[test]
    fn a_trie_range_outside_the_image_states_nothing() {
        assert_eq!(ExportIter::new(&GOBLIN_TRIE, Some(60..200), &[]).count(), 0);
    }
}
