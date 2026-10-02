//! `LC_DATA_IN_CODE` integration tests.
//!
//! The samples carry the load command with an empty table - clang emits it for
//! every arm64 and x86-64 image, and neither needs an entry - so the tests
//! point it at entries appended to a copy of the image.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::path::Path;

use darwinscope::{
    MachoBinary,
    dataincode::{DataInCode, DataInCodeKind},
};

const ARM64_PATH: &str = "tests/samples/synthesized/hello-cli/hello-arm64";

/// `LC_DATA_IN_CODE`.
const LC_DATA_IN_CODE: u32 = 0x29;

fn load() -> Vec<u8> {
    std::fs::read(Path::new(ARM64_PATH)).unwrap()
}

/// Byte offset of the `LC_DATA_IN_CODE` command in a thin 64-bit image.
fn data_in_code_command(image: &[u8]) -> usize {
    let ncmds = u32::from_le_bytes(image[16..20].try_into().unwrap());
    let mut at = 32;
    for _ in 0..ncmds {
        let cmd = u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
        let size = u32::from_le_bytes(image[at + 4..at + 8].try_into().unwrap());
        if cmd == LC_DATA_IN_CODE {
            return at;
        }
        at += size as usize;
    }
    panic!("sample has no LC_DATA_IN_CODE");
}

/// Appends `entries` to `image` and points its `LC_DATA_IN_CODE` at them.
fn with_entries(mut image: Vec<u8>, entries: &[(u32, u16, u16)]) -> Vec<u8> {
    let command = data_in_code_command(&image);
    let dataoff = u32::try_from(image.len()).unwrap();
    for &(offset, length, kind) in entries {
        image.extend_from_slice(&offset.to_le_bytes());
        image.extend_from_slice(&length.to_le_bytes());
        image.extend_from_slice(&kind.to_le_bytes());
    }
    let datasize = u32::try_from(entries.len() * 8).unwrap();
    image[command + 8..command + 12].copy_from_slice(&dataoff.to_le_bytes());
    image[command + 12..command + 16].copy_from_slice(&datasize.to_le_bytes());
    image
}

#[test]
fn an_empty_table_yields_nothing() {
    let bytes = load();
    let bin = MachoBinary::parse(&bytes).unwrap();
    assert_eq!(bin.data_in_code().count(), 0);
}

#[test]
fn entries_resolve_to_addresses_in_text() {
    // `_main` is at file offset 0x460 in `__TEXT`, which maps file offset 0
    // to 0x1_0000_0000.
    let bytes = with_entries(load(), &[(0x460, 0x10, 4), (0x470, 4, 1)]);
    let bin = MachoBinary::parse(&bytes).unwrap();
    let entries: Vec<DataInCode> = bin.data_in_code().collect();
    assert_eq!(
        entries,
        [
            DataInCode {
                offset: 0x460,
                address: Some(0x1_0000_0460),
                length: 0x10,
                kind: DataInCodeKind::JumpTable32,
            },
            DataInCode {
                offset: 0x470,
                address: Some(0x1_0000_0470),
                length: 4,
                kind: DataInCodeKind::Data,
            },
        ]
    );
    for entry in &entries {
        let address = entry.address.unwrap();
        assert_eq!(
            bin.vm_to_file_offset(address),
            Some(u64::from(entry.offset))
        );
    }
}

#[test]
fn a_table_past_the_image_yields_nothing() {
    let mut bytes = load();
    let command = data_in_code_command(&bytes);
    let past = u32::try_from(bytes.len()).unwrap();
    bytes[command + 8..command + 12].copy_from_slice(&past.to_le_bytes());
    bytes[command + 12..command + 16].copy_from_slice(&8_u32.to_le_bytes());
    let bin = MachoBinary::parse(&bytes).unwrap();
    assert_eq!(bin.data_in_code().count(), 0);
}
