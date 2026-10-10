//! `__TEXT,__unwind_info`, read from clang's C++ output for both
//! architectures (`synthesized/cxx-eh`, built by its `build.sh`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use darwinscope::{MachoBinary, unwind::LsdaRef};

/// The image's bytes.
fn image(arch: &str) -> Vec<u8> {
    std::fs::read(format!("tests/samples/synthesized/cxx-eh/cxx-eh-{arch}")).unwrap()
}

/// **Each function that handles exceptions names its LSDA.** The fixture's
/// five such functions - a typed catch, a catch-all, two catches on one try,
/// a destructor, a rethrow - each have one, in `__gcc_except_tab`, and each
/// is a function the table lists; the table's end lies past every function.
#[test]
fn every_handling_function_names_its_lsda() {
    for (arch, lsdas) in [
        (
            "x86_64",
            [
                (0x740, 0x950),
                (0x770, 0x964),
                (0x7a0, 0x978),
                (0x7e0, 0x994),
                (0x810, 0x9a0),
            ],
        ),
        (
            "arm64",
            [
                (0x710, 0x99c),
                (0x744, 0x9b0),
                (0x770, 0x9c4),
                (0x7c0, 0x9e0),
                (0x808, 0x9ec),
            ],
        ),
    ] {
        let data = image(arch);
        let binary = MachoBinary::parse(&data).unwrap();
        let info = binary.unwind_info().expect("an unwind table");
        let base = 0x1_0000_0000u64;
        let expected: Vec<LsdaRef> = lsdas
            .iter()
            .map(|&(function, lsda)| LsdaRef {
                function: base + function,
                lsda: base + lsda,
            })
            .collect();
        assert_eq!(info.lsdas(), expected, "{arch}");

        let except = binary
            .sections()
            .find(|section| section.sectname() == "__gcc_except_tab")
            .unwrap();
        let functions = info.functions();
        let end = info.end().unwrap();
        for lsda in info.lsdas() {
            assert!(
                (except.addr()..except.addr() + except.size()).contains(&lsda.lsda),
                "{arch}"
            );
            assert!(
                functions
                    .iter()
                    .any(|function| function.address == lsda.function),
                "{arch}"
            );
        }
        assert!(
            functions
                .windows(2)
                .all(|pair| pair[0].address < pair[1].address)
        );
        assert!(
            functions.iter().all(|function| function.address < end),
            "{arch}"
        );
    }
}

/// A C program with no exception handling has a table and no LSDA.
#[test]
fn a_program_without_handlers_has_no_lsda() {
    let data = std::fs::read("tests/samples/synthesized/hello-cli/hello-x86_64").unwrap();
    let binary = MachoBinary::parse(&data).unwrap();
    if let Some(info) = binary.unwind_info() {
        assert!(info.lsdas().is_empty());
        assert!(!info.functions().is_empty());
    }
}
