//! Imports - dyld bind targets.
//!
//! Decodes both binding encodings the toolchain emits:
//!
//! - **Legacy** `LC_DYLD_INFO` / `LC_DYLD_INFO_ONLY` bind-opcode
//!   streams (the non-lazy `bind_off` stream, then the lazy
//!   `lazy_bind_off` stream), interpreted here by `BindStream`.
//! - **Chained fixups** (`LC_DYLD_CHAINED_FIXUPS`, now the default
//!   on iOS / macOS) - decoded in [`crate::fixup`] and folded in
//!   here.
//!
//! ## Merge order
//!
//! The combined iterator yields **legacy bind rows first, then
//! chained-bind rows**, in load-command order. Real binaries ship
//! exactly one of the two encodings (never both), so de-duplication
//! is unnecessary - the order matters only as a documented
//! invariant for consumers, not as a meaningful sort.
//!
//! ## The bind-opcode interpreter
//!
//! `BindStream` runs the opcode state machine dyld runs
//! (cite: `dyld/src/ImageLoaderMachOCompressed.cpp` `eachBind` /
//! `eachLazyBind`), lazily: one row per bind, produced as the
//! consumer pulls it, so a repeat opcode costs only what is read.
//! It refuses what dyld refuses instead of trusting the input:
//!
//! - a bind into a segment the image does not have, or to a slot
//!   that does not fit inside its segment, ends the stream (dyld
//!   rejects the image there); this also bounds every repeat count
//!   by the segment it binds into;
//! - an opcode dyld does not know, or the threaded-bind opcode whose
//!   rows are slots of a chain this interpreter does not walk, ends
//!   the stream;
//! - `BIND_OPCODE_DONE` ends the non-lazy stream and separates the
//!   lazy stream's per-symbol sequences (each starting from a fresh
//!   state);
//! - library ordinals, including the special self / main-executable
//!   / flat-lookup ordinals of `BIND_OPCODE_SET_DYLIB_SPECIAL_IMM`,
//!   resolve through the image's dylib ordinal table
//!   (`fixup::resolve_lib_ordinal`).
//!
//! ## Lifetime
//!
//! `Import` carries a single lifetime `'p`: names and dylib paths
//! are zero-copy reborrows from the input data, and an [`Import`]
//! cannot outlive the [`MachoBinary`](crate::binary::MachoBinary)
//! borrow that produced it.

use core::ops::Range;
use std::sync::Arc;

use goblin::mach::bind_opcodes::{
    BIND_IMMEDIATE_MASK, BIND_OPCODE_ADD_ADDR_ULEB, BIND_OPCODE_DO_BIND,
    BIND_OPCODE_DO_BIND_ADD_ADDR_IMM_SCALED, BIND_OPCODE_DO_BIND_ADD_ADDR_ULEB,
    BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB, BIND_OPCODE_DONE, BIND_OPCODE_MASK,
    BIND_OPCODE_SET_ADDEND_SLEB, BIND_OPCODE_SET_DYLIB_ORDINAL_IMM,
    BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB, BIND_OPCODE_SET_DYLIB_SPECIAL_IMM,
    BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB, BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM,
    BIND_OPCODE_SET_TYPE_IMM, BIND_SYMBOL_FLAGS_WEAK_IMPORT,
};

use crate::{
    fixup::resolve_lib_ordinal,
    util::{read_cstr_at, read_sleb128, read_uleb128},
};

/// View over a single dyld bind target.
///
/// One [`Import`] is one slot the dynamic linker patches at load
/// time with a symbol resolved from another image. The same logical
/// row exists in both encodings dyld accepts:
///
/// - Legacy `BIND_OPCODE_*` stream from
///   `LC_DYLD_INFO[_ONLY].bind_off / lazy_bind_off /
///   weak_bind_off` (cite: `dyld/src/ImageLoaderMachOCompressed.cpp`).
/// - Chained binds from `LC_DYLD_CHAINED_FIXUPS` (cite:
///   `dyld/include/mach-o/fixup-chains.h`'s `dyld_chained_import*`
///   structs). See [`crate::fixup::Bind`] for the chained-form view.
///
/// Both forms are normalised into this single shape - consumers do
/// not need to know which encoding the binary used.
#[derive(Debug, Clone)]
pub struct Import<'p> {
    /// Symbol name dyld will resolve (e.g. `_objc_msgSend`).
    /// Zero-copy reborrow of the binary's symbol-pool / string-pool
    /// bytes.
    pub name: &'p str,
    /// `LC_LOAD_*_DYLIB` install-name path the symbol resolves into.
    /// Two-level lookup - dyld will not search other dylibs.
    pub dylib: &'p str,
    /// Whether resolution is deferred until first use
    /// (`BIND_OPCODE_DO_BIND_ULEB_TIMES_LAZY` / lazy bind table).
    /// `false` for chained-bind rows: chained fixups have no
    /// lazy/non-lazy distinction at the encoding level.
    pub is_lazy: bool,
    /// Weak-import flag (`BIND_SYMBOL_FLAGS_WEAK_IMPORT`). When
    /// `true`, dyld tolerates the symbol being missing at runtime
    /// and writes `0` into the slot.
    pub is_weak: bool,
    /// File offset of the slot to bind. `0` for slots that have no
    /// on-disk backing (BSS / non-lazy in zerofill segments).
    pub offset: u64,
    /// Slot size in bytes - `8` for ordinary 64-bit lazy / non-lazy
    /// pointers, `0` for rebase-only entries that the legacy decoder
    /// emits without a typed size.
    pub size: usize,
    /// VM address of the slot itself (`segment.vmaddr +
    /// segment_offset`).
    pub address: u64,
    /// Constant added to the resolved value (for indirect data
    /// pointers, vtable thunks, etc.). Always `0` for ordinary
    /// function-pointer binds.
    pub addend: i64,
    /// Byte offset within the legacy bind-opcode stream where this
    /// row originated. Useful for tracing back to the raw opcodes
    /// during forensic analysis. Always `0` for chained-bind rows
    /// (chained fixups have no opcode stream).
    pub bind_offset: u64,
}

/// Iterator over [`Import`] rows.
///
/// Combines legacy bind-opcode imports (interpreted lazily by
/// `BindStream`, the non-lazy stream then the lazy one) and
/// chained-fixup binds (decoded in-house by [`crate::fixup`]).
/// Yields legacy rows first, then chained rows. Empty when the
/// binary has no imports at all.
pub struct ImportIter<'p> {
    /// The legacy bind-opcode streams still to run, in order.
    legacy: std::vec::IntoIter<BindStream<'p>>,
    /// The stream being read.
    current: Option<BindStream<'p>>,
    /// The chained-fixup rows, yielded after every legacy row.
    chained: std::vec::IntoIter<Import<'p>>,
}

impl<'p> ImportIter<'p> {
    /// Yields every row of `legacy` in order, then `chained`.
    pub fn new(legacy: Vec<BindStream<'p>>, chained: Vec<Import<'p>>) -> Self {
        Self {
            legacy: legacy.into_iter(),
            current: None,
            chained: chained.into_iter(),
        }
    }
}

impl<'p> Iterator for ImportIter<'p> {
    type Item = Import<'p>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.current.as_mut().and_then(Iterator::next) {
                return Some(row);
            }
            match self.legacy.next() {
                Some(stream) => self.current = Some(stream),
                None => return self.chained.next(),
            }
        }
    }
}

/// One segment as a bind slot is placed in it.
#[derive(Debug, Clone, Copy)]
pub struct BindSegment {
    /// The segment's VM address.
    pub vmaddr: u64,
    /// The segment's VM size: every slot bound into it lies inside.
    pub vmsize: u64,
    /// The segment's file offset.
    pub fileoff: u64,
    /// The segment's file size: a slot past it has no on-disk bytes.
    pub filesize: u64,
}

/// What the image a [`BindStream`] runs over provides it.
#[derive(Debug, Clone)]
pub struct BindImage<'p> {
    /// The image's bytes.
    pub data: &'p [u8],
    /// Every `LC_SEGMENT[_64]` in load-command order (the bind
    /// opcodes' segment index).
    pub segments: Vec<BindSegment>,
    /// The dylib ordinal table
    /// ([`dylib_ordinals`](crate::dylib::dylib_ordinals)).
    pub dylibs: Vec<&'p str>,
    /// The pointer width in bytes, the size of every bound slot.
    pub pointer_size: u64,
}

/// The bind state the opcodes set before a bind.
#[derive(Debug, Clone, Copy, Default)]
struct BindState<'p> {
    /// The library ordinal the symbol binds from.
    ordinal: i64,
    /// The symbol name.
    name: &'p str,
    /// The `BIND_SYMBOL_FLAGS_*` immediate.
    flags: u8,
    /// The constant added to the resolved value.
    addend: i64,
    /// The segment index the slot lies in, `None` until set.
    segment: Option<usize>,
    /// The slot's offset inside its segment.
    segment_offset: u64,
}

/// A lazy interpreter over one bind-opcode stream (the non-lazy or
/// the lazy one): an iterator of the [`Import`] rows the stream binds.
#[derive(Debug, Clone)]
pub struct BindStream<'p> {
    /// The image the stream binds into (shared by both streams).
    image: Arc<BindImage<'p>>,
    /// The stream's bytes.
    stream: &'p [u8],
    /// Whether this is the lazy stream.
    is_lazy: bool,
    /// The next opcode's offset in `stream`.
    at: usize,
    /// The offset of the current sequence's first opcode.
    sequence: usize,
    /// The bind state.
    state: BindState<'p>,
    /// Binds still to emit for the current bind opcode, with the
    /// address step after each.
    pending: Option<(u64, u64)>,
    /// Set once the stream ended (or was refused).
    done: bool,
}

impl<'p> BindStream<'p> {
    /// The interpreter over `range` of the image's bytes. A range
    /// outside the image yields nothing.
    pub fn new(image: Arc<BindImage<'p>>, range: Range<usize>, is_lazy: bool) -> Self {
        let stream = image.data.get(range).unwrap_or_default();
        Self {
            image,
            stream,
            is_lazy,
            at: 0,
            sequence: 0,
            state: BindState::default(),
            pending: None,
            done: false,
        }
    }

    /// Reads a ULEB128 at the cursor and advances past it.
    fn uleb(&mut self) -> Option<u64> {
        let (value, size) = read_uleb128(self.stream.get(self.at..)?)?;
        self.at = self.at.checked_add(size)?;
        Some(value)
    }

    /// Reads a SLEB128 at the cursor and advances past it.
    fn sleb(&mut self) -> Option<i64> {
        let (value, size) = read_sleb128(self.stream.get(self.at..)?)?;
        self.at = self.at.checked_add(size)?;
        Some(value)
    }

    /// The row binding the current state's slot, then the slot
    /// advanced by `step`. `None` (the stream ends) when the slot does
    /// not fit inside a segment of the image, as dyld refuses it.
    fn bind(&mut self, step: u64) -> Option<Import<'p>> {
        let image = &self.image;
        let segment = image.segments.get(self.state.segment?)?;
        let offset = self.state.segment_offset;
        if offset.checked_add(image.pointer_size)? > segment.vmsize {
            return None;
        }
        let row = Import {
            name: self.state.name,
            dylib: resolve_lib_ordinal(self.state.ordinal, &image.dylibs),
            is_lazy: self.is_lazy,
            is_weak: self.state.flags & BIND_SYMBOL_FLAGS_WEAK_IMPORT != 0,
            offset: if offset < segment.filesize {
                segment.fileoff.checked_add(offset)?
            } else {
                0
            },
            size: usize::try_from(image.pointer_size).ok()?,
            address: segment.vmaddr.checked_add(offset)?,
            addend: self.state.addend,
            bind_offset: u64::try_from(self.sequence).ok()?,
        };
        self.state.segment_offset = offset.wrapping_add(step);
        Some(row)
    }

    /// Runs opcodes until the next bind, returning its row. `None`
    /// when the stream ends, or at the first opcode dyld would refuse.
    fn step(&mut self) -> Option<Import<'p>> {
        let pointer = self.image.pointer_size;
        if let Some((remaining, step)) = self.pending {
            self.pending = remaining
                .checked_sub(1)
                .filter(|&n| n > 0)
                .map(|n| (n, step));
            return self.bind(step);
        }
        loop {
            let opcode = *self.stream.get(self.at)?;
            self.at = self.at.checked_add(1)?;
            let immediate = opcode & BIND_IMMEDIATE_MASK;
            match opcode & BIND_OPCODE_MASK {
                BIND_OPCODE_DONE => {
                    if !self.is_lazy {
                        return None;
                    }
                    self.state = BindState::default();
                    self.sequence = self.at;
                }
                BIND_OPCODE_SET_DYLIB_ORDINAL_IMM => self.state.ordinal = i64::from(immediate),
                BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB => {
                    self.state.ordinal = i64::try_from(self.uleb()?).unwrap_or(i64::MAX);
                }
                BIND_OPCODE_SET_DYLIB_SPECIAL_IMM => {
                    // A special ordinal is the immediate sign-extended
                    // from four bits: 0 self, -1 main executable, -2
                    // flat lookup, -3 weak lookup.
                    self.state.ordinal = if immediate == 0 {
                        0
                    } else {
                        i64::from((immediate | BIND_OPCODE_MASK) as i8)
                    };
                }
                BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM => {
                    let name = read_cstr_at(self.stream, self.at)?;
                    self.at = self.at.checked_add(name.len())?.checked_add(1)?;
                    self.state.name = name;
                    self.state.flags = immediate;
                }
                BIND_OPCODE_SET_TYPE_IMM => {}
                BIND_OPCODE_SET_ADDEND_SLEB => self.state.addend = self.sleb()?,
                BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                    self.state.segment = Some(usize::from(immediate));
                    self.state.segment_offset = self.uleb()?;
                }
                BIND_OPCODE_ADD_ADDR_ULEB => {
                    let delta = self.uleb()?;
                    self.state.segment_offset = self.state.segment_offset.wrapping_add(delta);
                }
                BIND_OPCODE_DO_BIND => return self.bind(pointer),
                BIND_OPCODE_DO_BIND_ADD_ADDR_ULEB => {
                    let delta = self.uleb()?;
                    return self.bind(delta.wrapping_add(pointer));
                }
                BIND_OPCODE_DO_BIND_ADD_ADDR_IMM_SCALED => {
                    let scaled = u64::from(immediate).wrapping_mul(pointer);
                    return self.bind(scaled.wrapping_add(pointer));
                }
                BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB => {
                    let count = self.uleb()?;
                    let step = self.uleb()?.wrapping_add(pointer);
                    if count == 0 {
                        continue;
                    }
                    self.pending = count.checked_sub(1).filter(|&n| n > 0).map(|n| (n, step));
                    return self.bind(step);
                }
                // BIND_OPCODE_THREADED binds slots of a pointer chain
                // this interpreter does not walk; any other value is
                // an opcode dyld refuses.
                _ => return None,
            }
        }
    }
}

impl<'p> Iterator for BindStream<'p> {
    type Item = Import<'p>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let row = self.step();
        if row.is_none() {
            self.done = true;
        }
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-bit image with one 0x40-byte segment at 0x1000 (file
    /// offset 0x400, 0x20 bytes on disk) and one dylib.
    fn image(stream: &[u8]) -> Arc<BindImage<'_>> {
        Arc::new(BindImage {
            data: stream,
            segments: vec![BindSegment {
                vmaddr: 0x1000,
                vmsize: 0x40,
                fileoff: 0x400,
                filesize: 0x20,
            }],
            dylibs: vec!["/usr/lib/libSystem.B.dylib"],
            pointer_size: 8,
        })
    }

    /// Every row the stream binds, as `(name, dylib, address)`.
    fn rows(stream: &[u8], is_lazy: bool) -> Vec<(&str, &str, u64)> {
        BindStream::new(image(stream), 0..stream.len(), is_lazy)
            .map(|row| (row.name, row.dylib, row.address))
            .collect()
    }

    /// Opcodes binding `name` from `ordinal` at segment 0 offset
    /// `offset`.
    fn bind(ordinal: u8, name: &str, offset: u8) -> Vec<u8> {
        let mut ops = vec![BIND_OPCODE_SET_DYLIB_ORDINAL_IMM | ordinal];
        ops.push(BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM);
        ops.extend_from_slice(name.as_bytes());
        ops.push(0);
        ops.extend_from_slice(&[
            BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB,
            offset,
            BIND_OPCODE_DO_BIND,
        ]);
        ops
    }

    #[test]
    fn a_bind_states_its_slot_and_library() {
        let stream = bind(1, "_puts", 0x08);
        let row = BindStream::new(image(&stream), 0..stream.len(), false)
            .next()
            .unwrap();
        assert_eq!(
            (row.name, row.dylib, row.address, row.offset, row.size),
            ("_puts", "/usr/lib/libSystem.B.dylib", 0x1008, 0x408, 8)
        );
    }

    #[test]
    fn a_slot_past_the_file_bytes_has_no_file_offset() {
        let stream = bind(1, "_x", 0x30);
        let row = BindStream::new(image(&stream), 0..stream.len(), false)
            .next()
            .unwrap();
        assert_eq!((row.address, row.offset), (0x1030, 0));
    }

    #[test]
    fn an_ordinal_no_load_command_holds_binds_from_no_library() {
        assert_eq!(rows(&bind(9, "_x", 0), false), [("_x", "", 0x1000)]);
    }

    #[test]
    fn a_special_ordinal_names_its_lookup() {
        let mut stream = bind(1, "_x", 0);
        stream[0] = BIND_OPCODE_SET_DYLIB_SPECIAL_IMM | 0x0e;
        assert_eq!(rows(&stream, false), [("_x", "flat-namespace", 0x1000)]);
        stream[0] = BIND_OPCODE_SET_DYLIB_SPECIAL_IMM | 0x0f;
        assert_eq!(rows(&stream, false), [("_x", "main-executable", 0x1000)]);
        stream[0] = BIND_OPCODE_SET_DYLIB_SPECIAL_IMM;
        assert_eq!(rows(&stream, false), [("_x", "self", 0x1000)]);
    }

    #[test]
    fn a_segment_the_image_lacks_ends_the_stream() {
        let mut stream = bind(1, "_a", 0);
        stream.extend_from_slice(&[
            BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | 3,
            0x00,
            BIND_OPCODE_DO_BIND,
        ]);
        stream.extend_from_slice(&bind(1, "_b", 0x10));
        assert_eq!(
            rows(&stream, false),
            [("_a", "/usr/lib/libSystem.B.dylib", 0x1000)]
        );
    }

    #[test]
    fn a_repeat_count_is_bounded_by_its_segment() {
        // Bind 2^63 times, skipping nothing: the 0x40-byte segment
        // holds eight 8-byte slots.
        let mut stream = bind(1, "_x", 0);
        stream.pop();
        stream.push(BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB);
        stream.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01]);
        stream.push(0x00);
        let addresses: Vec<u64> = rows(&stream, false).into_iter().map(|r| r.2).collect();
        assert_eq!(
            addresses,
            (0..8).map(|i| 0x1000 + i * 8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn done_ends_the_non_lazy_stream_and_separates_lazy_sequences() {
        let mut stream = bind(1, "_a", 0);
        stream.push(BIND_OPCODE_DONE);
        let second = stream.len();
        stream.extend_from_slice(&bind(1, "_b", 0x08));
        stream.push(BIND_OPCODE_DONE);
        assert_eq!(rows(&stream, false).len(), 1);
        let lazy: Vec<(&str, u64)> = BindStream::new(image(&stream), 0..stream.len(), true)
            .map(|row| (row.name, row.bind_offset))
            .collect();
        assert_eq!(lazy, [("_a", 0), ("_b", second as u64)]);
    }

    #[test]
    fn a_lazy_sequence_starts_from_a_fresh_state() {
        // The second sequence binds without setting a segment: dyld's
        // fresh state has none, so the stream ends there.
        let mut stream = bind(1, "_a", 0);
        stream.extend_from_slice(&[BIND_OPCODE_DONE, BIND_OPCODE_DO_BIND]);
        assert_eq!(rows(&stream, true).len(), 1);
    }

    #[test]
    fn an_opcode_dyld_refuses_ends_the_stream() {
        for refused in [0xd0u8, 0xe0, 0xf0] {
            let mut stream = bind(1, "_a", 0);
            stream.push(refused);
            stream.extend_from_slice(&bind(1, "_b", 0x08));
            assert_eq!(rows(&stream, false).len(), 1, "{refused:#x}");
        }
    }

    #[test]
    fn a_truncated_stream_keeps_the_rows_before_it() {
        let mut stream = bind(1, "_a", 0);
        stream.extend_from_slice(&[BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM, b'_', b'b']);
        assert_eq!(rows(&stream, false).len(), 1);
    }
}
