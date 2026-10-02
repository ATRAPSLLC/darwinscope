//! `LC_DATA_IN_CODE`: the linker's account of data inside code sections.
//!
//! A code section is not all instructions. Jump tables and literal islands are
//! laid out between functions, and on 32-bit ARM inside them, and a
//! disassembler that decodes them reads garbage and, worse, follows it. The
//! linker records every such range it placed in an `LC_DATA_IN_CODE` table: an
//! array of fixed-size `data_in_code_entry` records in `__LINKEDIT`, each an
//! offset, a length and a `DICE_KIND_*` value.
//!
//! [`MachoBinary::data_in_code`](crate::MachoBinary::data_in_code) walks that
//! table and resolves each entry's offset to the virtual address its segment
//! maps it to, since the address is what a disassembler keys on.

use core::marker::PhantomData;

use crate::util::file_offset_to_vm_in;

/// Size of one on-disk `data_in_code_entry`: a `u32` offset, a `u16` length
/// and a `u16` kind.
const SIZEOF_DATA_IN_CODE_ENTRY: usize = 8;

/// What an `LC_DATA_IN_CODE` range holds: the entry's `DICE_KIND_*` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataInCodeKind {
    /// `DICE_KIND_DATA`: data of no stated shape, such as a literal pool.
    Data,
    /// `DICE_KIND_JUMP_TABLE8`: a jump table of 1-byte entries.
    JumpTable8,
    /// `DICE_KIND_JUMP_TABLE16`: a jump table of 2-byte entries.
    JumpTable16,
    /// `DICE_KIND_JUMP_TABLE32`: a jump table of 4-byte entries.
    JumpTable32,
    /// `DICE_KIND_ABS_JUMP_TABLE32`: a table of absolute 4-byte addresses.
    AbsJumpTable32,
    /// A kind this crate does not know, kept as stored.
    ///
    /// Kept rather than dropped: the entry still says its range is not
    /// instructions, whatever shape the data has.
    Other(u16),
}

impl DataInCodeKind {
    /// Reads a stored `DICE_KIND_*` value.
    ///
    /// # Arguments
    ///
    /// * `raw` - The entry's `kind` field.
    #[must_use]
    pub const fn from_raw(raw: u16) -> Self {
        match raw {
            1 => Self::Data,
            2 => Self::JumpTable8,
            3 => Self::JumpTable16,
            4 => Self::JumpTable32,
            5 => Self::AbsJumpTable32,
            other => Self::Other(other),
        }
    }

    /// The width of one table entry, for the table kinds.
    ///
    /// `None` for [`Self::Data`] and for a kind this crate does not know: the
    /// range holds data, but not in a stated table shape.
    #[must_use]
    pub const fn entry_bytes(self) -> Option<u8> {
        match self {
            Self::JumpTable8 => Some(1),
            Self::JumpTable16 => Some(2),
            Self::JumpTable32 | Self::AbsJumpTable32 => Some(4),
            Self::Data | Self::Other(_) => None,
        }
    }
}

/// One `LC_DATA_IN_CODE` entry: a range of a code section holding data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DataInCode {
    /// Offset of the range's first byte from the image's `mach_header`, as
    /// stored.
    pub offset: u32,
    /// The virtual address `offset` maps to through the segments' file
    /// extents, or `None` when no segment's on-disk bytes hold it - a
    /// malformed or hostile entry, kept so the caller sees it.
    pub address: Option<u64>,
    /// The range's length in bytes.
    pub length: u16,
    /// What the range holds.
    pub kind: DataInCodeKind,
}

/// Iterator over the entries of `LC_DATA_IN_CODE`.
///
/// Returned by [`MachoBinary::data_in_code`](crate::MachoBinary::data_in_code).
/// Yields entries in table order; a trailing partial entry is ignored.
pub struct DataInCodeIter<'a, 'p> {
    stream: &'a [u8],
    cursor: usize,
    little_endian: bool,
    /// `(vmaddr, vmsize, fileoff, filesize)` per segment, for resolving each
    /// entry's offset to an address.
    segments: Vec<(u64, u64, u64, u64)>,
    _parent: PhantomData<&'p ()>,
}

impl<'a, 'p> DataInCodeIter<'a, 'p> {
    /// Creates an iterator over `stream`, the table's bytes.
    ///
    /// # Arguments
    ///
    /// * `stream` - The `dataoff..dataoff + datasize` slice of the image.
    /// * `little_endian` - The image's byte order, which the entries share.
    /// * `segments` - `(vmaddr, vmsize, fileoff, filesize)` per segment.
    pub(crate) fn new(
        stream: &'a [u8],
        little_endian: bool,
        segments: Vec<(u64, u64, u64, u64)>,
    ) -> Self {
        Self {
            stream,
            cursor: 0,
            little_endian,
            segments,
            _parent: PhantomData,
        }
    }

    /// An iterator that yields nothing.
    pub(crate) fn empty() -> Self {
        Self::new(&[], true, Vec::new())
    }
}

impl<'a, 'p> Iterator for DataInCodeIter<'a, 'p> {
    type Item = DataInCode;

    fn next(&mut self) -> Option<DataInCode> {
        let end = self.cursor.checked_add(SIZEOF_DATA_IN_CODE_ENTRY)?;
        let record = self.stream.get(self.cursor..end)?;
        self.cursor = end;
        let (offset, length, kind) = match *record {
            [o0, o1, o2, o3, l0, l1, k0, k1] if self.little_endian => (
                u32::from_le_bytes([o0, o1, o2, o3]),
                u16::from_le_bytes([l0, l1]),
                u16::from_le_bytes([k0, k1]),
            ),
            [o0, o1, o2, o3, l0, l1, k0, k1] => (
                u32::from_be_bytes([o0, o1, o2, o3]),
                u16::from_be_bytes([l0, l1]),
                u16::from_be_bytes([k0, k1]),
            ),
            _ => return None,
        };
        Some(DataInCode {
            offset,
            address: file_offset_to_vm_in(self.segments.iter().copied(), u64::from(offset)),
            length,
            kind: DataInCodeKind::from_raw(kind),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{DataInCode, DataInCodeIter, DataInCodeKind};

    /// Every known kind reads back, table kinds with their entry width, and an
    /// unknown kind is kept rather than dropped.
    #[test]
    fn kinds_read_with_their_entry_width() {
        let expected = [
            (1, DataInCodeKind::Data, None),
            (2, DataInCodeKind::JumpTable8, Some(1)),
            (3, DataInCodeKind::JumpTable16, Some(2)),
            (4, DataInCodeKind::JumpTable32, Some(4)),
            (5, DataInCodeKind::AbsJumpTable32, Some(4)),
            (0x77, DataInCodeKind::Other(0x77), None),
        ];
        for (raw, kind, width) in expected {
            assert_eq!(DataInCodeKind::from_raw(raw), kind);
            assert_eq!(kind.entry_bytes(), width);
        }
    }

    /// Entries decode in the image's byte order, resolve through the segment
    /// holding their offset, and a trailing partial entry ends the walk.
    #[test]
    fn entries_decode_and_resolve() {
        // __TEXT: vmaddr 0x1_0000_0000, 0x4000 bytes from file offset 0.
        let segments = vec![(0x1_0000_0000, 0x4000, 0, 0x4000)];
        let mut little = Vec::new();
        little.extend_from_slice(&0x3f10_u32.to_le_bytes());
        little.extend_from_slice(&0x20_u16.to_le_bytes());
        little.extend_from_slice(&4_u16.to_le_bytes());
        little.extend_from_slice(&0x9000_u32.to_le_bytes());
        little.extend_from_slice(&8_u16.to_le_bytes());
        little.extend_from_slice(&1_u16.to_le_bytes());
        little.extend_from_slice(&[0xAA; 5]);
        let entries: Vec<DataInCode> =
            DataInCodeIter::new(&little, true, segments.clone()).collect();
        assert_eq!(
            entries,
            [
                DataInCode {
                    offset: 0x3f10,
                    address: Some(0x1_0000_3f10),
                    length: 0x20,
                    kind: DataInCodeKind::JumpTable32,
                },
                // Past every segment's file extent: kept, with no address.
                DataInCode {
                    offset: 0x9000,
                    address: None,
                    length: 8,
                    kind: DataInCodeKind::Data,
                },
            ]
        );

        let mut big = Vec::new();
        big.extend_from_slice(&0x3f10_u32.to_be_bytes());
        big.extend_from_slice(&0x20_u16.to_be_bytes());
        big.extend_from_slice(&2_u16.to_be_bytes());
        let entries: Vec<DataInCode> = DataInCodeIter::new(&big, false, segments).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].address, Some(0x1_0000_3f10));
        assert_eq!(entries[0].kind, DataInCodeKind::JumpTable8);
    }
}
