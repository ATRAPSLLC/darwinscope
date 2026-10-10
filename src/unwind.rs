//! `__TEXT,__unwind_info`: the compact unwind table.
//!
//! The linker folds every function's unwind description into one table: a
//! first-level index of `{function, second-level page, LSDA range}` entries,
//! second-level pages listing each function's start and 32-bit encoding
//! (regular pages verbatim, compressed pages as a 24-bit offset from the page's
//! base and an 8-bit index into the common and page-local encoding arrays),
//! and one array of `{function, LSDA}` pairs for the functions with
//! exception-handling data. Every address is an offset from the image's
//! `mach_header`, which is the `__TEXT` segment's `vmaddr`.
//!
//! [`MachoBinary::unwind_info`](crate::MachoBinary::unwind_info) finds the
//! section; [`UnwindInfo::functions`] and [`UnwindInfo::lsdas`] read it. The
//! last first-level entry is a sentinel at the end of the code, so a
//! function's extent runs to the next start.

use crate::util::read_u32_le_at;

/// Bytes of the section header: seven `u32`s.
const HEADER_BYTES: usize = 28;
/// Bytes of one first-level index entry.
const INDEX_ENTRY_BYTES: usize = 12;
/// Bytes of one LSDA descriptor: `{function, lsda}`.
const LSDA_ENTRY_BYTES: usize = 8;
/// A regular second-level page: `{function, encoding}` entries.
const REGULAR_PAGE: u32 = 2;
/// A compressed second-level page: packed `u32` entries.
const COMPRESSED_PAGE: u32 = 3;

/// One function the table describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnwindFunction {
    /// The function's first instruction, as a virtual address.
    pub address: u64,
    /// Its compact unwind encoding (`UNWIND_*`, architecture-specific).
    pub encoding: u32,
}

/// A function's language-specific data area: its exception-handling table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LsdaRef {
    /// The function's first instruction, as a virtual address.
    pub function: u64,
    /// The LSDA, as a virtual address (in `__gcc_except_tab`).
    pub lsda: u64,
}

/// A view of a `__unwind_info` section.
#[derive(Debug, Clone, Copy)]
pub struct UnwindInfo<'a> {
    /// The section's bytes.
    bytes: &'a [u8],
    /// The image's `mach_header` address, which every offset counts from.
    image_base: u64,
}

impl<'a> UnwindInfo<'a> {
    /// Reads `bytes` as a version 1 table of an image whose `mach_header` is
    /// at `image_base`; `None` for any other version or a short header.
    #[must_use]
    pub fn parse(bytes: &'a [u8], image_base: u64) -> Option<Self> {
        if bytes.len() < HEADER_BYTES || read_u32_le_at(bytes, 0)? != 1 {
            return None;
        }
        Some(Self { bytes, image_base })
    }

    /// The `u32` at `offset` in the section.
    fn word(&self, offset: usize) -> Option<u32> {
        read_u32_le_at(self.bytes, offset)
    }

    /// The `u16` at `offset` in the section.
    fn half(&self, offset: usize) -> Option<u16> {
        Some(u16::from_le_bytes(
            self.bytes
                .get(offset..offset.checked_add(2)?)?
                .try_into()
                .ok()?,
        ))
    }

    /// The first-level index entries: `{function offset, second-level page
    /// offset, LSDA descriptor offset}`, the last a sentinel.
    fn index(&self) -> Vec<(u32, u32, u32)> {
        let (Some(at), Some(count)) = (self.word(20), self.word(24)) else {
            return Vec::new();
        };
        (0..count as usize)
            .map_while(|index| {
                let entry = (at as usize).checked_add(index.checked_mul(INDEX_ENTRY_BYTES)?)?;
                Some((
                    self.word(entry)?,
                    self.word(entry.checked_add(4)?)?,
                    self.word(entry.checked_add(8)?)?,
                ))
            })
            .collect()
    }

    /// The address an offset from the `mach_header` names.
    fn address(&self, offset: u32) -> u64 {
        self.image_base.wrapping_add(u64::from(offset))
    }

    /// Every function the second-level pages list, in address order, with its
    /// encoding; the first-level sentinel, which ends the last function, is
    /// not one. A page that does not read contributes nothing.
    #[must_use]
    pub fn functions(&self) -> Vec<UnwindFunction> {
        let common_at = self.word(4).unwrap_or(0) as usize;
        let common_count = self.word(8).unwrap_or(0);
        let common = |index: u32| -> Option<u32> {
            (index < common_count)
                .then(|| self.word(common_at.checked_add((index as usize).checked_mul(4)?)?))
                .flatten()
        };
        let mut functions = Vec::new();
        for (base, page, _) in self.index() {
            if page == 0 {
                continue;
            }
            let page = page as usize;
            match self.word(page) {
                Some(REGULAR_PAGE) => {
                    let (Some(at), Some(count)) = (
                        page.checked_add(4).and_then(|at| self.half(at)),
                        page.checked_add(6).and_then(|at| self.half(at)),
                    ) else {
                        continue;
                    };
                    for index in 0..usize::from(count) {
                        let Some(entry) = page
                            .checked_add(usize::from(at))
                            .and_then(|entries| entries.checked_add(index.checked_mul(8)?))
                        else {
                            break;
                        };
                        let (Some(function), Some(encoding)) = (
                            self.word(entry),
                            entry.checked_add(4).and_then(|at| self.word(at)),
                        ) else {
                            break;
                        };
                        functions.push(UnwindFunction {
                            address: self.address(function),
                            encoding,
                        });
                    }
                }
                Some(COMPRESSED_PAGE) => {
                    let header =
                        |offset: usize| page.checked_add(offset).and_then(|at| self.half(at));
                    let (Some(at), Some(count), Some(local_at), Some(local_count)) =
                        (header(4), header(6), header(8), header(10))
                    else {
                        continue;
                    };
                    for index in 0..usize::from(count) {
                        let Some(packed) = page
                            .checked_add(usize::from(at))
                            .and_then(|entries| entries.checked_add(index.checked_mul(4)?))
                            .and_then(|entry| self.word(entry))
                        else {
                            break;
                        };
                        let encoding_index = packed >> 24;
                        // The common encodings first, then the page's own.
                        let encoding = common(encoding_index).or_else(|| {
                            let local = encoding_index.checked_sub(common_count)?;
                            (local < u32::from(local_count))
                                .then(|| {
                                    self.word(
                                        page.checked_add(usize::from(local_at))?
                                            .checked_add((local as usize).checked_mul(4)?)?,
                                    )
                                })
                                .flatten()
                        });
                        let Some(encoding) = encoding else {
                            continue;
                        };
                        functions.push(UnwindFunction {
                            address: self.address(base.wrapping_add(packed & 0x00FF_FFFF)),
                            encoding,
                        });
                    }
                }
                _ => {}
            }
        }
        functions.sort_by_key(|function| function.address);
        functions.dedup_by_key(|function| function.address);
        functions
    }

    /// The address one past the last function the table describes: its
    /// first-level sentinel.
    #[must_use]
    pub fn end(&self) -> Option<u64> {
        self.index().last().map(|&(end, _, _)| self.address(end))
    }

    /// Every function with an LSDA, and the LSDA, in table order: the
    /// descriptors from the first first-level entry's LSDA offset to the
    /// sentinel's.
    #[must_use]
    pub fn lsdas(&self) -> Vec<LsdaRef> {
        let index = self.index();
        let (Some(&(_, _, first)), Some(&(_, _, last))) = (index.first(), index.last()) else {
            return Vec::new();
        };
        (first as usize..last as usize)
            .step_by(LSDA_ENTRY_BYTES)
            .map_while(|entry| {
                Some(LsdaRef {
                    function: self.address(self.word(entry)?),
                    lsda: self.address(self.word(entry.checked_add(4)?)?),
                })
            })
            .collect()
    }
}
