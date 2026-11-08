//! Swift 5 type-metadata walker.
//!
//! Walks every reflection section the Swift runtime emits into a
//! Mach-O image:
//!
//! - `__TEXT,__swift5_types` - i32-relative pointer array of
//!   [`TypeDescriptor`]s (`TargetTypeContextDescriptor` per
//!   `swift/include/swift/ABI/Metadata.h:4025-4138`). Class / struct
//!   / enum kinds; per-kind tail decoders surface vtable, override
//!   table, resilient superclass, foreign / singleton metadata
//!   initialisation, prespecialisations, invertible protocols, and
//!   singleton-metadata-pointer trailing objects.
//! - `__TEXT,__swift5_protos` - i32-relative pointer array of
//!   [`SwiftProtocol`]s (`TargetProtocolDescriptor`).
//! - `__TEXT,__swift5_proto` - packed array of [`Conformance`] rows
//!   (`TargetProtocolConformanceDescriptor`). Resolves the
//!   `(type_descriptor, protocol_descriptor, witness_table, flags)`
//!   triple per [`crate::swift::ConformanceFlags`].
//! - `__TEXT,__swift5_fieldmd` - variable-length sequence of
//!   [`FieldDescriptor`]s, each carrying `NumFields` x
//!   [`FieldRecord`] entries (mangled type name + reflstr-hosted
//!   field name).
//! - `__TEXT,__swift5_replac` / `__swift5_replac2` -
//!   dynamic-replacement scope descriptors / chain entries.
//! - `__TEXT,__swift5_capture` - closure-capture descriptors.
//! - `__TEXT,__swift5_reflstr` - pool of NUL-terminated UTF-8 names
//!   referenced by [`FieldRecord::field_name`].
//! - `__TEXT,__swift5_typeref` / `__swift5_builtin` - mangled type
//!   reference pool / builtin-type layout records (presence-only).
//!
//! All names are stored in their **mangled** form. The schema
//! reserves a separate slot for a future demangler.
//!
//! See `RESEARCH.md` §"Swift type metadata" (lines 1696-2480) for
//! layout references. Section name synonyms are catalogued in
//! `RESEARCH.md` §"Section-name catalogue" - the lookup is
//! segment-agnostic on purpose.
//!
//! ## Lifetime convention
//!
//! Every typed view here uses `<'a, 'p>` - `'a` is the data slice
//! lifetime (where mangled names, field strings, and section bodies
//! live) and `'p` is the borrow of the parent [`SwiftRuntime`]. Swift
//! string fields all live in `__TEXT,__swift5_reflstr` /
//! `__swift5_typeref` / type-descriptor-local string blobs and are
//! addressable through [`MachoBinary::raw`], so `&'a str` is the
//! correct name lifetime - matching the [`crate::objc::ObjcRuntime`]
//! convention.
//!
//! [`MachoBinary`]: crate::binary::MachoBinary
//! [`MachoBinary::raw`]: crate::binary::MachoBinary::raw
//!
//! ## Fail-soft posture
//!
//! [`MachoBinary::swift`](crate::binary::MachoBinary::swift) returns
//! `None` when the image carries no Swift content - concretely, when
//! none of `__swift5_types`, `__swift5_protos`, `__swift5_proto`,
//! `__swift5_fieldmd` is present. A reflection-stripped Swift binary
//! that ships only conformances still produces `Some(_)`.
//!
//! Per-row decode failures inside variable-length tables (type
//! descriptors, field records, vtable entries) silently skip the
//! row; when the `tracing` feature is enabled the bail-out is
//! emitted at debug level. Unknown enum kinds surface as
//! [`ContextDescriptorKind::Other`] / [`TypeReferenceKind::Other`] /
//! [`FieldDescriptorKind::Other`] / [`SwiftMethodKind::Other`]
//! rather than dropping the row.

use crate::{
    binary::MachoBinary,
    util::{read_cstr_at, vm_to_file_offset_in},
};

mod capture;
mod classtrailers;
mod conformance;
mod context;
mod field;
mod parent;
mod protocol;
mod reflstr;
mod replacement;
mod section;
mod typedescriptor;
mod vtable;

pub use capture::{CaptureDescriptor, CaptureIter};
pub use classtrailers::{
    ForeignMetadataInit, GenericContextHeader, InvertibleProtocolSet, ObjcResilientClassStubInfo,
    PrespecializationIter, ResilientSuperclass, SingletonMetadataInit, SingletonMetadataPointer,
    decode_class_trailers, decode_value_type_trailers,
};
pub use conformance::{Conformance, ConformanceIter, TypeReference};
pub use context::{
    ConformanceFlags, ContextDescriptorFlags, ContextDescriptorKind, FieldDescriptorKind,
    FieldRecordFlags, MetadataInitializationKind, MethodDescriptorFlags, SwiftMethodKind,
    TypeContextDescriptorFlags, TypeReferenceKind,
};
pub use field::{FieldDescriptor, FieldIter, FieldRecord, FieldRecordIter};
pub use parent::{ParentChain, ParentContext};
pub use protocol::{ProtocolIter, SwiftProtocol};
pub use reflstr::lookup_field_name;
pub use replacement::{DynamicReplacementScope, ReplacementIter};
pub use typedescriptor::{
    ClassBody, DefaultOverrideTableHeader, EnumBody, OverrideTableHeader, StructBody,
    TypeDescriptor, TypeIter, TypeKindBody, VTableHeader,
};
pub use vtable::{
    DefaultOverrideEntry, DefaultOverrideEntryIter, OverrideEntry, OverrideEntryIter, VTableEntry,
    VTableIter,
};

pub use section::{SwiftSection, find_swift_section};

/// Aggregate view over the Swift 5 runtime metadata embedded in a
/// Mach-O image.
///
/// Constructed via
/// [`MachoBinary::swift`](crate::binary::MachoBinary::swift); returns
/// `None` when the image carries no Swift content (none of
/// `__swift5_types`, `__swift5_protos`, `__swift5_proto`, or
/// `__swift5_fieldmd` is present) or when the parsed Mach-O is
/// 32-bit (the v0.1 walker is 64-bit only).
///
/// Carries the parsed-once metadata by value (cached section
/// lookups, segment table for `vm_to_file_offset`, chained-fixup
/// rebase / bind index) so iterators can borrow `&self`
/// independently of the originating [`MachoBinary`] borrow.
///
/// Cite: `RESEARCH.md` §"Swift type metadata" (line 1696),
/// §"Section-name catalogue" (line 2369).
#[derive(Debug)]
pub struct SwiftRuntime<'a> {
    data: &'a [u8],
    /// Cached `(vmaddr, vmsize, fileoff, filesize)` tuples for every
    /// segment with non-zero file backing - the input to
    /// [`vm_to_file_offset_in`]. Kept by value so the runtime
    /// outlives the originating [`MachoBinary`] borrow.
    segments: Vec<(u64, u64, u64, u64)>,

    types: Option<SwiftSection<'a>>,
    protos: Option<SwiftSection<'a>>,
    proto: Option<SwiftSection<'a>>,
    fieldmd: Option<SwiftSection<'a>>,
    replac: Option<SwiftSection<'a>>,
    replac2: Option<SwiftSection<'a>>,
    capture: Option<SwiftSection<'a>>,
    /// `__swift5_reflstr` - pool of NUL-terminated UTF-8 reflection
    /// strings. Field-name resolution falls back here when the
    /// primary segment-table lookup fails (forward-compat for
    /// linker variants that emit reflstr outside the standard
    /// `__TEXT` placement).
    reflstr: Option<SwiftSection<'a>>,
    /// `__swift5_typeref` - mangled-name pool. Presence-only,
    /// reported by [`Self::has_mangled_name_pool`]; the descriptors'
    /// own relative name pointers are resolved without it.
    typeref: Option<SwiftSection<'a>>,
    builtin: Option<SwiftSection<'a>>,
    entry: Option<SwiftSection<'a>>,
    mpenum: Option<SwiftSection<'a>>,
    acfuncs: Option<SwiftSection<'a>>,
    assocty: Option<SwiftSection<'a>>,
}

impl<'a> SwiftRuntime<'a> {
    /// Construct the aggregate from a parent [`MachoBinary`].
    ///
    /// Returns `None` when:
    ///
    /// - The image is 32-bit. v0.1 deliberately scopes the Swift
    ///   walker to 64-bit Mach-O; 32-bit slices return `None` here
    ///   so the caller can record a single skip-with-reason event
    ///   without having to inspect every accessor.
    /// - The image carries no Swift content - concretely, none of
    ///   `__swift5_types`, `__swift5_protos`, `__swift5_proto`, or
    ///   `__swift5_fieldmd` is present. Reflection-stripped images
    ///   that ship only conformances still produce `Some(_)`.
    pub fn build(bin: &MachoBinary<'a>) -> Option<Self> {
        if !bin.header().is_64() {
            #[cfg(feature = "tracing")]
            tracing::debug!("darwinscope::swift: 32-bit Mach-O - Swift walker is 64-bit only");
            return None;
        }

        let types = find_swift_section(bin, "__swift5_types");
        let protos = find_swift_section(bin, "__swift5_protos");
        let proto = find_swift_section(bin, "__swift5_proto");
        let fieldmd = find_swift_section(bin, "__swift5_fieldmd");

        // Detector union - any of the four "load-bearing" Swift
        // sections counts as Swift content. A binary can be stripped
        // of fieldmd reflection but still carry types + conformances;
        // a third-party library can ship only conformances against
        // an external type. The unioned detector covers every
        // real-world case.
        if types.is_none() && protos.is_none() && proto.is_none() && fieldmd.is_none() {
            return None;
        }

        let mut segments: Vec<(u64, u64, u64, u64)> = Vec::new();
        for s in bin.segments() {
            segments.push((s.vmaddr(), s.vmsize(), s.fileoff(), s.filesize()));
        }

        Some(Self {
            data: bin.raw(),
            segments,
            types,
            protos,
            proto,
            fieldmd,
            replac: find_swift_section(bin, "__swift5_replac"),
            replac2: find_swift_section(bin, "__swift5_replac2"),
            capture: find_swift_section(bin, "__swift5_capture"),
            reflstr: find_swift_section(bin, "__swift5_reflstr"),
            typeref: find_swift_section(bin, "__swift5_typeref"),
            builtin: find_swift_section(bin, "__swift5_builtin"),
            entry: find_swift_section(bin, "__swift5_entry"),
            mpenum: find_swift_section(bin, "__swift5_mpenum"),
            acfuncs: find_swift_section(bin, "__swift5_acfuncs"),
            assocty: find_swift_section(bin, "__swift5_assocty"),
        })
    }

    /// Iterator over every type context descriptor in
    /// `__swift5_types` - class, struct, enum, plus any other kind
    /// surfaced as [`ContextDescriptorKind::Other`].
    pub fn types(&self) -> TypeIter<'a, '_> {
        TypeIter::new(self)
    }

    /// Iterator over every protocol descriptor in `__swift5_protos`.
    pub fn protocols(&self) -> ProtocolIter<'a, '_> {
        ProtocolIter::new(self)
    }

    /// Iterator over every protocol-conformance row in
    /// `__swift5_proto`. The single most attribution-bearing Swift
    /// section.
    pub fn conformances(&self) -> ConformanceIter<'a, '_> {
        ConformanceIter::new(self)
    }

    /// Iterator over every field descriptor in `__swift5_fieldmd`.
    /// Each [`FieldDescriptor`] yields its own [`FieldRecord`]
    /// stream.
    pub fn field_descriptors(&self) -> FieldIter<'a, '_> {
        FieldIter::new(self)
    }

    /// Iterator over `__swift5_replac` dynamic-replacement scope
    /// descriptors.
    pub fn dynamic_replacements(&self) -> ReplacementIter<'a, '_> {
        ReplacementIter::new(self)
    }

    /// Iterator over `__swift5_capture` closure-capture descriptors.
    pub fn captures(&self) -> CaptureIter<'a, '_> {
        CaptureIter::new(self)
    }

    /// `true` when the image carries an `__swift5_entry` section
    /// (Swift `@main` entry-point info).
    pub fn has_entry_point(&self) -> bool {
        self.entry.is_some()
    }

    /// `true` when the image carries an `__swift5_typeref` section
    /// (the mangled-name pool the descriptors' relative name
    /// pointers reach into). Presence-only.
    pub fn has_mangled_name_pool(&self) -> bool {
        self.typeref.is_some()
    }

    /// `true` when the image carries an `__swift5_builtin` section.
    /// Presence-only - the structured walker is post-v0.1.
    pub fn has_builtin_descriptors(&self) -> bool {
        self.builtin.is_some()
    }

    /// `true` when the image carries an `__swift5_mpenum` section
    /// (multi-payload enum spare-bit info). Presence-only.
    pub fn has_multi_payload_enum_descriptors(&self) -> bool {
        self.mpenum.is_some()
    }

    /// `true` when the image carries an `__swift5_acfuncs` section
    /// (`@_silgen_name` accessible-function table, Swift 5.7+).
    /// Presence-only.
    pub fn has_accessible_functions(&self) -> bool {
        self.acfuncs.is_some()
    }

    /// `true` when the image carries an `__swift5_assocty` section
    /// (associated-type witnesses). Presence-only.
    pub fn has_associated_type_descriptors(&self) -> bool {
        self.assocty.is_some()
    }

    /// `true` when the image carries an `__swift5_replac2` section
    /// (Swift 5.5+ dynamic replacement chain). Presence-only.
    pub fn has_replacement_chain(&self) -> bool {
        self.replac2.is_some()
    }

    /// Translate a virtual-memory address to its on-disk file
    /// offset using the cached segment table.
    fn vm_to_file_offset(&self, vmaddr: u64) -> Option<u64> {
        vm_to_file_offset_in(self.segments.iter().copied(), vmaddr)
    }

    /// Read the C-string at virtual address `vmaddr`. Returns `None`
    /// when the address fails to resolve through the segment table
    /// or when the bytes are not valid UTF-8.
    fn read_cstr(&self, vmaddr: u64) -> Option<&'a str> {
        if vmaddr == 0 {
            return None;
        }
        let off = self.vm_to_file_offset(vmaddr)? as usize;
        read_cstr_at(self.data, off)
    }

    /// Translate a VA into a file offset and return a slice of `len`
    /// bytes from that offset, or `None` on out-of-bounds.
    fn read_bytes(&self, vmaddr: u64, len: usize) -> Option<&'a [u8]> {
        let off = self.vm_to_file_offset(vmaddr)? as usize;
        let end = off.checked_add(len)?;
        self.data.get(off..end)
    }
}
