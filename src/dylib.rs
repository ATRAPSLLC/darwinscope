//! Dylib graph (`LC_LOAD_*_DYLIB`) and load-command summary.
//!
//! Two iterators ride on top of the parsed `MachO.load_commands`
//! vector:
//!
//! - [`DylibIter`] - every load command of kind `LC_LOAD_DYLIB`,
//!   `LC_LOAD_WEAK_DYLIB`, `LC_REEXPORT_DYLIB`,
//!   `LC_LOAD_UPWARD_DYLIB`, or `LC_LAZY_LOAD_DYLIB`.
//! - [`LoadCommandIter`] - flat summary of every load command (one
//!   row per `LC_*`), including the `cmd` id, byte offset, size,
//!   and a `&'a [u8]` slice over the command's raw bytes.
//!
//! `LC_ID_DYLIB` (a self-identifying dylib's own `install_name`) is
//! deliberately *not* surfaced through [`DylibIter`] - it does not
//! describe a dependency. Use [`LoadCommandIter`] to find it if
//! needed.

use core::marker::PhantomData;

use goblin::mach::load_command::{
    CommandVariant, DylibCommand, LoadCommand as GoblinLoadCommand, cmd_to_str,
};

use crate::binary::{Version, read_lc_str};

/// View over a single dylib dependency.
#[derive(Debug)]
pub struct Dylib<'a, 'p> {
    /// Path / install_name of the dylib (e.g.
    /// `/usr/lib/libSystem.B.dylib`).
    pub name: &'a str,
    /// What kind of load (regular, weak, re-export, …).
    pub kind: DylibKind,
    /// `dylib.timestamp` - historically a build timestamp; modern
    /// images frequently zero this for reproducible-build reasons.
    pub timestamp: u32,
    /// Current version of the dylib.
    pub current_version: Version,
    /// Compatibility version that consumers must be at least.
    pub compat_version: Version,
    _parent: PhantomData<&'p ()>,
}

/// Which `LC_LOAD_*_DYLIB` variant introduced this dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DylibKind {
    /// `LC_LOAD_DYLIB` - regular, must resolve at load time.
    Load,
    /// `LC_LOAD_WEAK_DYLIB` - weak; missing dylib does not abort.
    LoadWeak,
    /// `LC_REEXPORT_DYLIB` - re-export the loaded dylib's symbols.
    Reexport,
    /// `LC_LOAD_UPWARD_DYLIB` - upward dependency (cycle break).
    LoadUpward,
    /// `LC_LAZY_LOAD_DYLIB` - defer load until first reference.
    LazyLoad,
}

/// Iterator over [`Dylib`] dependencies in load-command order.
pub struct DylibIter<'a, 'p> {
    data: &'a [u8],
    inner: core::slice::Iter<'p, GoblinLoadCommand>,
}

impl<'a, 'p> DylibIter<'a, 'p> {
    /// The iterator over the dependency load commands of `lcs`,
    /// reading install names out of `data`.
    pub fn new(data: &'a [u8], lcs: &'p [GoblinLoadCommand]) -> Self {
        Self {
            data,
            inner: lcs.iter(),
        }
    }
}

impl<'a, 'p> Iterator for DylibIter<'a, 'p> {
    type Item = Dylib<'a, 'p>;
    fn next(&mut self) -> Option<Self::Item> {
        for lc in self.inner.by_ref() {
            // Skip LC_ID_DYLIB and everything else.
            let Some((kind, c)) = dylib_command(lc) else {
                continue;
            };
            let Some(name) = resolve_dylib_name(self.data, lc, c) else {
                // Malformed: name offset out of bounds. Fail-soft.
                continue;
            };
            return Some(Dylib {
                name,
                kind,
                timestamp: c.dylib.timestamp,
                current_version: Version::from_packed_u32(c.dylib.current_version),
                compat_version: Version::from_packed_u32(c.dylib.compatibility_version),
                _parent: PhantomData,
            });
        }
        None
    }
}

/// The dependency a load command declares, with its kind: `None` for
/// every command but the five `LC_*_DYLIB` dependency kinds (so also
/// for `LC_ID_DYLIB`, the image's own install name).
fn dylib_command(lc: &GoblinLoadCommand) -> Option<(DylibKind, &DylibCommand)> {
    match &lc.command {
        CommandVariant::LoadDylib(c) => Some((DylibKind::Load, c)),
        CommandVariant::LoadWeakDylib(c) => Some((DylibKind::LoadWeak, c)),
        CommandVariant::ReexportDylib(c) => Some((DylibKind::Reexport, c)),
        CommandVariant::LoadUpwardDylib(c) => Some((DylibKind::LoadUpward, c)),
        CommandVariant::LazyLoadDylib(c) => Some((DylibKind::LazyLoad, c)),
        _ => None,
    }
}

/// The image's dylib ordinal table: one install name per dependency
/// load command, in load-command order, so entry `n - 1` is library
/// ordinal `n` as dyld numbers it.
///
/// A command whose name cannot be read keeps its position as `""`:
/// dyld numbers ordinals by load command, so dropping it would shift
/// every later ordinal onto the wrong library. Resolve an ordinal
/// through [`resolve_lib_ordinal`](crate::fixup::resolve_lib_ordinal).
pub fn dylib_ordinals<'a>(data: &'a [u8], lcs: &[GoblinLoadCommand]) -> Vec<&'a str> {
    lcs.iter()
        .filter_map(|lc| {
            let (_, c) = dylib_command(lc)?;
            Some(resolve_dylib_name(data, lc, c).unwrap_or(""))
        })
        .collect()
}

/// Reads a dependency command's install name, bounded by the command.
fn resolve_dylib_name<'a>(
    data: &'a [u8],
    lc: &GoblinLoadCommand,
    c: &DylibCommand,
) -> Option<&'a str> {
    let name_off = c.dylib.name as usize;
    let total_off = lc.offset.checked_add(name_off)?;
    let end = lc.offset.checked_add(lc.command.cmdsize())?;
    read_lc_str(data, total_off, end)
}

/// Summary view over one load command.
#[derive(Debug)]
pub struct LoadCommand<'a, 'p> {
    /// `LC_*` identifier.
    pub kind: u32,
    /// Byte offset of this load command inside the binary.
    pub offset: usize,
    /// Total size in bytes (`cmdsize`).
    pub size: u32,
    /// The command's raw bytes (`data[offset..offset+size]`). Empty
    /// when bounds are violated by a malformed image.
    pub bytes: &'a [u8],
    _parent: PhantomData<&'p ()>,
}

impl<'a, 'p> LoadCommand<'a, 'p> {
    /// Human-readable `LC_*` name (e.g. `"LC_SEGMENT_64"`). Returns
    /// `"LC_UNKNOWN"` for values goblin does not recognise.
    pub fn name(&self) -> &'static str {
        cmd_to_str(self.kind)
    }
}

/// Iterator over every load command in load-command order.
pub struct LoadCommandIter<'a, 'p> {
    data: &'a [u8],
    inner: core::slice::Iter<'p, GoblinLoadCommand>,
}

impl<'a, 'p> LoadCommandIter<'a, 'p> {
    /// The iterator over every load command in `lcs`, reading any
    /// string payload out of `data`.
    pub fn new(data: &'a [u8], lcs: &'p [GoblinLoadCommand]) -> Self {
        Self {
            data,
            inner: lcs.iter(),
        }
    }
}

impl<'a, 'p> Iterator for LoadCommandIter<'a, 'p> {
    type Item = LoadCommand<'a, 'p>;
    fn next(&mut self) -> Option<Self::Item> {
        let lc = self.inner.next()?;
        let kind = lc.command.cmd();
        let cmdsize = lc.command.cmdsize();
        let bytes = lc
            .offset
            .checked_add(cmdsize)
            .and_then(|end| self.data.get(lc.offset..end))
            .unwrap_or(&[]);
        Some(LoadCommand {
            kind,
            offset: lc.offset,
            size: cmdsize as u32,
            bytes,
            _parent: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use goblin::mach::load_command::{
        Dylib as GoblinDylib, LC_ID_DYLIB, LC_LOAD_DYLIB, LC_LOAD_WEAK_DYLIB,
    };

    use super::*;

    /// A `LC_LOAD_DYLIB`-shaped command of kind `cmd` whose
    /// `dylib.name` offset is `name_off`, placed at `offset` in the
    /// image and `cmdsize` bytes long.
    fn command(cmd: u32, offset: usize, cmdsize: u32, name_off: u32) -> GoblinLoadCommand {
        GoblinLoadCommand {
            offset,
            command: CommandVariant::LoadDylib(DylibCommand {
                cmd,
                cmdsize,
                dylib: GoblinDylib {
                    name: name_off,
                    timestamp: 0,
                    current_version: 0,
                    compatibility_version: 0,
                },
            }),
        }
    }

    /// The same command shape under another `CommandVariant`, so the
    /// kind-dispatch of `dylib_command` is what decides.
    fn variant(v: CommandVariant) -> GoblinLoadCommand {
        GoblinLoadCommand {
            offset: 0,
            command: v,
        }
    }

    /// `data` holding `/a.dylib` at 24 and `/b.dylib` at 56, the two
    /// commands starting at 0 and 32 with a 24-byte header.
    fn image() -> Vec<u8> {
        let mut data = vec![0u8; 32];
        data[24..].copy_from_slice(b"/a.dylib");
        data.resize(56, 0);
        data.extend_from_slice(b"/b.dylib\0");
        data.resize(96, 0);
        data
    }

    #[test]
    fn the_table_holds_one_entry_per_dependency_command_in_order() {
        let data = image();
        let lcs = [
            command(LC_LOAD_DYLIB, 0, 32, 24),
            command(LC_LOAD_WEAK_DYLIB, 32, 32, 24),
        ];
        assert_eq!(dylib_ordinals(&data, &lcs), ["/a.dylib", "/b.dylib"]);
    }

    #[test]
    fn a_command_whose_name_is_unreadable_keeps_its_ordinal_slot() {
        // The first command's name offset runs past its own cmdsize,
        // so the name cannot be read. Dropping the row would make
        // ordinal 1 resolve to the second library.
        let data = image();
        let lcs = [
            command(LC_LOAD_DYLIB, 0, 8, 24),
            command(LC_LOAD_WEAK_DYLIB, 32, 32, 24),
        ];
        let table = dylib_ordinals(&data, &lcs);
        assert_eq!(table, ["", "/b.dylib"]);
        // Ordinals are 1-based into this table.
        assert_eq!(crate::fixup::resolve_lib_ordinal(2, &table), "/b.dylib");
    }

    #[test]
    fn a_command_that_is_not_a_dependency_takes_no_ordinal() {
        // LC_ID_DYLIB is the image's own install name, not a
        // dependency: dyld does not number it.
        let data = image();
        let lcs = [
            variant(CommandVariant::IdDylib(DylibCommand {
                cmd: LC_ID_DYLIB,
                cmdsize: 32,
                dylib: GoblinDylib {
                    name: 24,
                    timestamp: 0,
                    current_version: 0,
                    compatibility_version: 0,
                },
            })),
            command(LC_LOAD_DYLIB, 32, 32, 24),
        ];
        assert_eq!(dylib_ordinals(&data, &lcs), ["/b.dylib"]);
    }
}
