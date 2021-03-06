// Decoding metadata from a single crate's metadata

use crate::creader::CrateMetadataRef;
use crate::rmeta::table::{FixedSizeEncoding, Table};
use crate::rmeta::*;

use rustc_ast::ast;
use rustc_attr as attr;
use rustc_data_structures::captures::Captures;
use rustc_data_structures::fingerprint::Fingerprint;
use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::svh::Svh;
use rustc_data_structures::sync::{AtomicCell, Lock, LockGuard, Lrc, OnceCell};
use rustc_expand::base::{SyntaxExtension, SyntaxExtensionKind};
use rustc_expand::proc_macro::{AttrProcMacro, BangProcMacro, ProcMacroDerive};
use rustc_hir as hir;
use rustc_hir::def::{CtorKind, CtorOf, DefKind, Res};
use rustc_hir::def_id::{CrateNum, DefId, DefIndex, LocalDefId, CRATE_DEF_INDEX, LOCAL_CRATE};
use rustc_hir::definitions::DefPathTable;
use rustc_hir::definitions::{DefKey, DefPath, DefPathData, DefPathHash};
use rustc_hir::lang_items;
use rustc_index::vec::{Idx, IndexVec};
use rustc_middle::dep_graph::{self, DepNode, DepNodeExt, DepNodeIndex};
use rustc_middle::hir::exports::Export;
use rustc_middle::middle::cstore::{CrateSource, ExternCrate};
use rustc_middle::middle::cstore::{ForeignModule, LinkagePreference, NativeLib};
use rustc_middle::middle::exported_symbols::{ExportedSymbol, SymbolExportLevel};
use rustc_middle::mir::interpret::{AllocDecodingSession, AllocDecodingState};
use rustc_middle::mir::{self, interpret, Body, Promoted};
use rustc_middle::ty::codec::TyDecoder;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_middle::util::common::record_time;
use rustc_serialize::{opaque, Decodable, Decoder, SpecializedDecoder, UseSpecializedDecodable};
use rustc_session::Session;
use rustc_span::hygiene::ExpnDataDecodeMode;
use rustc_span::source_map::{respan, Spanned};
use rustc_span::symbol::{sym, Ident, Symbol};
use rustc_span::{self, hygiene::MacroKind, BytePos, ExpnId, Pos, Span, SyntaxContext, DUMMY_SP};

use log::debug;
use proc_macro::bridge::client::ProcMacro;
use std::cell::Cell;
use std::io;
use std::mem;
use std::num::NonZeroUsize;
use std::path::Path;

pub use cstore_impl::{provide, provide_extern};
use rustc_span::hygiene::HygieneDecodeContext;

mod cstore_impl;

crate struct MetadataBlob(MetadataRef);

// A map from external crate numbers (as decoded from some crate file) to
// local crate numbers (as generated during this session). Each external
// crate may refer to types in other external crates, and each has their
// own crate numbers.
crate type CrateNumMap = IndexVec<CrateNum, CrateNum>;

crate struct CrateMetadata {
    /// The primary crate data - binary metadata blob.
    blob: MetadataBlob,

    // --- Some data pre-decoded from the metadata blob, usually for performance ---
    /// Properties of the whole crate.
    /// NOTE(eddyb) we pass `'static` to a `'tcx` parameter because this
    /// lifetime is only used behind `Lazy`, and therefore acts like an
    /// universal (`for<'tcx>`), that is paired up with whichever `TyCtxt`
    /// is being used to decode those values.
    root: CrateRoot<'static>,
    /// For each definition in this crate, we encode a key. When the
    /// crate is loaded, we read all the keys and put them in this
    /// hashmap, which gives the reverse mapping. This allows us to
    /// quickly retrace a `DefPath`, which is needed for incremental
    /// compilation support.
    def_path_table: DefPathTable,
    /// Trait impl data.
    /// FIXME: Used only from queries and can use query cache,
    /// so pre-decoding can probably be avoided.
    trait_impls: FxHashMap<(u32, DefIndex), Lazy<[DefIndex]>>,
    /// Proc macro descriptions for this crate, if it's a proc macro crate.
    raw_proc_macros: Option<&'static [ProcMacro]>,
    /// Source maps for code from the crate.
    source_map_import_info: OnceCell<Vec<ImportedSourceFile>>,
    /// Used for decoding interpret::AllocIds in a cached & thread-safe manner.
    alloc_decoding_state: AllocDecodingState,
    /// The `DepNodeIndex` of the `DepNode` representing this upstream crate.
    /// It is initialized on the first access in `get_crate_dep_node_index()`.
    /// Do not access the value directly, as it might not have been initialized yet.
    /// The field must always be initialized to `DepNodeIndex::INVALID`.
    dep_node_index: AtomicCell<DepNodeIndex>,

    // --- Other significant crate properties ---
    /// ID of this crate, from the current compilation session's point of view.
    cnum: CrateNum,
    /// Maps crate IDs as they are were seen from this crate's compilation sessions into
    /// IDs as they are seen from the current compilation session.
    cnum_map: CrateNumMap,
    /// Same ID set as `cnum_map` plus maybe some injected crates like panic runtime.
    dependencies: Lock<Vec<CrateNum>>,
    /// How to link (or not link) this crate to the currently compiled crate.
    dep_kind: Lock<CrateDepKind>,
    /// Filesystem location of this crate.
    source: CrateSource,
    /// Whether or not this crate should be consider a private dependency
    /// for purposes of the 'exported_private_dependencies' lint
    private_dep: bool,
    /// The hash for the host proc macro. Used to support `-Z dual-proc-macro`.
    host_hash: Option<Svh>,

    /// Additional data used for decoding `HygieneData` (e.g. `SyntaxContext`
    /// and `ExpnId`).
    /// Note that we store a `HygieneDecodeContext` for each `CrateMetadat`. This is
    /// because `SyntaxContext` ids are not globally unique, so we need
    /// to track which ids we've decoded on a per-crate basis.
    hygiene_context: HygieneDecodeContext,

    // --- Data used only for improving diagnostics ---
    /// Information about the `extern crate` item or path that caused this crate to be loaded.
    /// If this is `None`, then the crate was injected (e.g., by the allocator).
    extern_crate: Lock<Option<ExternCrate>>,
}

/// Holds information about a rustc_span::SourceFile imported from another crate.
/// See `imported_source_files()` for more information.
struct ImportedSourceFile {
    /// This SourceFile's byte-offset within the source_map of its original crate
    original_start_pos: rustc_span::BytePos,
    /// The end of this SourceFile within the source_map of its original crate
    original_end_pos: rustc_span::BytePos,
    /// The imported SourceFile's representation within the local source_map
    translated_source_file: Lrc<rustc_span::SourceFile>,
}

pub(super) struct DecodeContext<'a, 'tcx> {
    opaque: opaque::Decoder<'a>,
    cdata: Option<CrateMetadataRef<'a>>,
    sess: Option<&'tcx Session>,
    tcx: Option<TyCtxt<'tcx>>,

    // Cache the last used source_file for translating spans as an optimization.
    last_source_file_index: usize,

    lazy_state: LazyState,

    // Used for decoding interpret::AllocIds in a cached & thread-safe manner.
    alloc_decoding_session: Option<AllocDecodingSession<'a>>,
}

/// Abstract over the various ways one can create metadata decoders.
pub(super) trait Metadata<'a, 'tcx>: Copy {
    fn raw_bytes(self) -> &'a [u8];
    fn cdata(self) -> Option<CrateMetadataRef<'a>> {
        None
    }
    fn sess(self) -> Option<&'tcx Session> {
        None
    }
    fn tcx(self) -> Option<TyCtxt<'tcx>> {
        None
    }

    fn decoder(self, pos: usize) -> DecodeContext<'a, 'tcx> {
        let tcx = self.tcx();
        DecodeContext {
            opaque: opaque::Decoder::new(self.raw_bytes(), pos),
            cdata: self.cdata(),
            sess: self.sess().or(tcx.map(|tcx| tcx.sess)),
            tcx,
            last_source_file_index: 0,
            lazy_state: LazyState::NoNode,
            alloc_decoding_session: self
                .cdata()
                .map(|cdata| cdata.cdata.alloc_decoding_state.new_decoding_session()),
        }
    }
}

impl<'a, 'tcx> Metadata<'a, 'tcx> for &'a MetadataBlob {
    fn raw_bytes(self) -> &'a [u8] {
        &self.0
    }
}

impl<'a, 'tcx> Metadata<'a, 'tcx> for (&'a MetadataBlob, &'tcx Session) {
    fn raw_bytes(self) -> &'a [u8] {
        let (blob, _) = self;
        &blob.0
    }

    fn sess(self) -> Option<&'tcx Session> {
        let (_, sess) = self;
        Some(sess)
    }
}

impl<'a, 'tcx> Metadata<'a, 'tcx> for &'a CrateMetadataRef<'a> {
    fn raw_bytes(self) -> &'a [u8] {
        self.blob.raw_bytes()
    }
    fn cdata(self) -> Option<CrateMetadataRef<'a>> {
        Some(*self)
    }
}

impl<'a, 'tcx> Metadata<'a, 'tcx> for (&'a CrateMetadataRef<'a>, &'tcx Session) {
    fn raw_bytes(self) -> &'a [u8] {
        self.0.raw_bytes()
    }
    fn cdata(self) -> Option<CrateMetadataRef<'a>> {
        Some(*self.0)
    }
    fn sess(self) -> Option<&'tcx Session> {
        Some(&self.1)
    }
}

impl<'a, 'tcx> Metadata<'a, 'tcx> for (&'a CrateMetadataRef<'a>, TyCtxt<'tcx>) {
    fn raw_bytes(self) -> &'a [u8] {
        self.0.raw_bytes()
    }
    fn cdata(self) -> Option<CrateMetadataRef<'a>> {
        Some(*self.0)
    }
    fn tcx(self) -> Option<TyCtxt<'tcx>> {
        Some(self.1)
    }
}

impl<'a, 'tcx, T: Decodable> Lazy<T, ()> {
    fn decode<M: Metadata<'a, 'tcx>>(self, metadata: M) -> T {
        let mut dcx = metadata.decoder(self.position.get());
        dcx.lazy_state = LazyState::NodeStart(self.position);
        T::decode(&mut dcx).unwrap()
    }
}

impl<'a: 'x, 'tcx: 'x, 'x, T: Decodable> Lazy<[T], usize> {
    fn decode<M: Metadata<'a, 'tcx>>(
        self,
        metadata: M,
    ) -> impl ExactSizeIterator<Item = T> + Captures<'a> + Captures<'tcx> + 'x {
        let mut dcx = metadata.decoder(self.position.get());
        dcx.lazy_state = LazyState::NodeStart(self.position);
        (0..self.meta).map(move |_| T::decode(&mut dcx).unwrap())
    }
}

impl<'a, 'tcx> DecodeContext<'a, 'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx.expect("missing TyCtxt in DecodeContext")
    }

    fn cdata(&self) -> CrateMetadataRef<'a> {
        self.cdata.expect("missing CrateMetadata in DecodeContext")
    }

    fn read_lazy_with_meta<T: ?Sized + LazyMeta>(
        &mut self,
        meta: T::Meta,
    ) -> Result<Lazy<T>, <Self as Decoder>::Error> {
        let min_size = T::min_size(meta);
        let distance = self.read_usize()?;
        let position = match self.lazy_state {
            LazyState::NoNode => bug!("read_lazy_with_meta: outside of a metadata node"),
            LazyState::NodeStart(start) => {
                let start = start.get();
                assert!(distance + min_size <= start);
                start - distance - min_size
            }
            LazyState::Previous(last_min_end) => last_min_end.get() + distance,
        };
        self.lazy_state = LazyState::Previous(NonZeroUsize::new(position + min_size).unwrap());
        Ok(Lazy::from_position_and_meta(NonZeroUsize::new(position).unwrap(), meta))
    }
}

impl<'a, 'tcx> TyDecoder<'tcx> for DecodeContext<'a, 'tcx> {
    #[inline]
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx.expect("missing TyCtxt in DecodeContext")
    }

    #[inline]
    fn peek_byte(&self) -> u8 {
        self.opaque.data[self.opaque.position()]
    }

    #[inline]
    fn position(&self) -> usize {
        self.opaque.position()
    }

    fn cached_ty_for_shorthand<F>(
        &mut self,
        shorthand: usize,
        or_insert_with: F,
    ) -> Result<Ty<'tcx>, Self::Error>
    where
        F: FnOnce(&mut Self) -> Result<Ty<'tcx>, Self::Error>,
    {
        let tcx = self.tcx();

        let key = ty::CReaderCacheKey { cnum: self.cdata().cnum, pos: shorthand };

        if let Some(&ty) = tcx.ty_rcache.borrow().get(&key) {
            return Ok(ty);
        }

        let ty = or_insert_with(self)?;
        tcx.ty_rcache.borrow_mut().insert(key, ty);
        Ok(ty)
    }

    fn cached_predicate_for_shorthand<F>(
        &mut self,
        shorthand: usize,
        or_insert_with: F,
    ) -> Result<ty::Predicate<'tcx>, Self::Error>
    where
        F: FnOnce(&mut Self) -> Result<ty::Predicate<'tcx>, Self::Error>,
    {
        let tcx = self.tcx();

        let key = ty::CReaderCacheKey { cnum: self.cdata().cnum, pos: shorthand };

        if let Some(&pred) = tcx.pred_rcache.borrow().get(&key) {
            return Ok(pred);
        }

        let pred = or_insert_with(self)?;
        tcx.pred_rcache.borrow_mut().insert(key, pred);
        Ok(pred)
    }

    fn with_position<F, R>(&mut self, pos: usize, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        let new_opaque = opaque::Decoder::new(self.opaque.data, pos);
        let old_opaque = mem::replace(&mut self.opaque, new_opaque);
        let old_state = mem::replace(&mut self.lazy_state, LazyState::NoNode);
        let r = f(self);
        self.opaque = old_opaque;
        self.lazy_state = old_state;
        r
    }

    fn map_encoded_cnum_to_current(&self, cnum: CrateNum) -> CrateNum {
        if cnum == LOCAL_CRATE { self.cdata().cnum } else { self.cdata().cnum_map[cnum] }
    }
}

impl<'a, 'tcx, T> SpecializedDecoder<Lazy<T, ()>> for DecodeContext<'a, 'tcx> {
    fn specialized_decode(&mut self) -> Result<Lazy<T>, Self::Error> {
        self.read_lazy_with_meta(())
    }
}

impl<'a, 'tcx, T> SpecializedDecoder<Lazy<[T], usize>> for DecodeContext<'a, 'tcx> {
    fn specialized_decode(&mut self) -> Result<Lazy<[T]>, Self::Error> {
        let len = self.read_usize()?;
        if len == 0 { Ok(Lazy::empty()) } else { self.read_lazy_with_meta(len) }
    }
}

impl<'a, 'tcx, I: Idx, T> SpecializedDecoder<Lazy<Table<I, T>, usize>> for DecodeContext<'a, 'tcx>
where
    Option<T>: FixedSizeEncoding,
{
    fn specialized_decode(&mut self) -> Result<Lazy<Table<I, T>>, Self::Error> {
        let len = self.read_usize()?;
        self.read_lazy_with_meta(len)
    }
}

impl<'a, 'tcx> SpecializedDecoder<DefId> for DecodeContext<'a, 'tcx> {
    #[inline]
    fn specialized_decode(&mut self) -> Result<DefId, Self::Error> {
        let krate = CrateNum::decode(self)?;
        let index = DefIndex::decode(self)?;

        Ok(DefId { krate, index })
    }
}

impl<'a, 'tcx> SpecializedDecoder<DefIndex> for DecodeContext<'a, 'tcx> {
    #[inline]
    fn specialized_decode(&mut self) -> Result<DefIndex, Self::Error> {
        Ok(DefIndex::from_u32(self.read_u32()?))
    }
}

impl<'a, 'tcx> SpecializedDecoder<LocalDefId> for DecodeContext<'a, 'tcx> {
    #[inline]
    fn specialized_decode(&mut self) -> Result<LocalDefId, Self::Error> {
        Ok(DefId::decode(self)?.expect_local())
    }
}

impl<'a, 'tcx> SpecializedDecoder<interpret::AllocId> for DecodeContext<'a, 'tcx> {
    fn specialized_decode(&mut self) -> Result<interpret::AllocId, Self::Error> {
        if let Some(alloc_decoding_session) = self.alloc_decoding_session {
            alloc_decoding_session.decode_alloc_id(self)
        } else {
            bug!("Attempting to decode interpret::AllocId without CrateMetadata")
        }
    }
}

impl<'a, 'tcx> SpecializedDecoder<Span> for DecodeContext<'a, 'tcx> {
    fn specialized_decode(&mut self) -> Result<Span, Self::Error> {
        let tag = u8::decode(self)?;

        if tag == TAG_INVALID_SPAN {
            return Ok(DUMMY_SP);
        }

        debug_assert!(tag == TAG_VALID_SPAN_LOCAL || tag == TAG_VALID_SPAN_FOREIGN);

        let lo = BytePos::decode(self)?;
        let len = BytePos::decode(self)?;
        let ctxt = SyntaxContext::decode(self)?;
        let hi = lo + len;

        let sess = if let Some(sess) = self.sess {
            sess
        } else {
            bug!("Cannot decode Span without Session.")
        };

        // There are two possibilities here:
        // 1. This is a 'local span', which is located inside a `SourceFile`
        // that came from this crate. In this case, we use the source map data
        // encoded in this crate. This branch should be taken nearly all of the time.
        // 2. This is a 'foreign span', which is located inside a `SourceFile`
        // that came from a *different* crate (some crate upstream of the one
        // whose metadata we're looking at). For example, consider this dependency graph:
        //
        // A -> B -> C
        //
        // Suppose that we're currently compiling crate A, and start deserializing
        // metadata from crate B. When we deserialize a Span from crate B's metadata,
        // there are two posibilites:
        //
        // 1. The span references a file from crate B. This makes it a 'local' span,
        // which means that we can use crate B's serialized source map information.
        // 2. The span references a file from crate C. This makes it a 'foreign' span,
        // which means we need to use Crate *C* (not crate B) to determine the source
        // map information. We only record source map information for a file in the
        // crate that 'owns' it, so deserializing a Span may require us to look at
        // a transitive dependency.
        //
        // When we encode a foreign span, we adjust its 'lo' and 'high' values
        // to be based on the *foreign* crate (e.g. crate C), not the crate
        // we are writing metadata for (e.g. crate B). This allows us to
        // treat the 'local' and 'foreign' cases almost identically during deserialization:
        // we can call `imported_source_files` for the proper crate, and binary search
        // through the returned slice using our span.
        let imported_source_files = if tag == TAG_VALID_SPAN_LOCAL {
            self.cdata().imported_source_files(sess)
        } else {
            // When we encode a proc-macro crate, all `Span`s should be encoded
            // with `TAG_VALID_SPAN_LOCAL`
            if self.cdata().root.is_proc_macro_crate() {
                // Decode `CrateNum` as u32 - using `CrateNum::decode` will ICE
                // since we don't have `cnum_map` populated.
                let cnum = u32::decode(self)?;
                panic!(
                    "Decoding of crate {:?} tried to access proc-macro dep {:?}",
                    self.cdata().root.name,
                    cnum
                );
            }
            // tag is TAG_VALID_SPAN_FOREIGN, checked by `debug_assert` above
            let cnum = CrateNum::decode(self)?;
            debug!(
                "SpecializedDecoder<Span>::specialized_decode: loading source files from cnum {:?}",
                cnum
            );

            // Decoding 'foreign' spans should be rare enough that it's
            // not worth it to maintain a per-CrateNum cache for `last_source_file_index`.
            // We just set it to 0, to ensure that we don't try to access something out
            // of bounds for our initial 'guess'
            self.last_source_file_index = 0;

            let foreign_data = self.cdata().cstore.get_crate_data(cnum);
            foreign_data.imported_source_files(sess)
        };

        let source_file = {
            // Optimize for the case that most spans within a translated item
            // originate from the same source_file.
            let last_source_file = &imported_source_files[self.last_source_file_index];

            if lo >= last_source_file.original_start_pos && lo <= last_source_file.original_end_pos
            {
                last_source_file
            } else {
                let index = imported_source_files
                    .binary_search_by_key(&lo, |source_file| source_file.original_start_pos)
                    .unwrap_or_else(|index| index - 1);

                // Don't try to cache the index for foreign spans,
                // as this would require a map from CrateNums to indices
                if tag == TAG_VALID_SPAN_LOCAL {
                    self.last_source_file_index = index;
                }
                &imported_source_files[index]
            }
        };

        // Make sure our binary search above is correct.
        debug_assert!(
            lo >= source_file.original_start_pos && lo <= source_file.original_end_pos,
            "Bad binary search: lo={:?} source_file.original_start_pos={:?} source_file.original_end_pos={:?}",
            lo,
            source_file.original_start_pos,
            source_file.original_end_pos
        );

        // Make sure we correctly filtered out invalid spans during encoding
        debug_assert!(
            hi >= source_file.original_start_pos && hi <= source_file.original_end_pos,
            "Bad binary search: hi={:?} source_file.original_start_pos={:?} source_file.original_end_pos={:?}",
            hi,
            source_file.original_start_pos,
            source_file.original_end_pos
        );

        let lo =
            (lo + source_file.translated_source_file.start_pos) - source_file.original_start_pos;
        let hi =
            (hi + source_file.translated_source_file.start_pos) - source_file.original_start_pos;

        Ok(Span::new(lo, hi, ctxt))
    }
}

impl<'a, 'tcx> SpecializedDecoder<Fingerprint> for DecodeContext<'a, 'tcx> {
    fn specialized_decode(&mut self) -> Result<Fingerprint, Self::Error> {
        Fingerprint::decode_opaque(&mut self.opaque)
    }
}

impl<'a, 'tcx, T> SpecializedDecoder<mir::ClearCrossCrate<T>> for DecodeContext<'a, 'tcx>
where
    mir::ClearCrossCrate<T>: UseSpecializedDecodable,
{
    #[inline]
    fn specialized_decode(&mut self) -> Result<mir::ClearCrossCrate<T>, Self::Error> {
        Ok(mir::ClearCrossCrate::Clear)
    }
}

implement_ty_decoder!(DecodeContext<'a, 'tcx>);

impl MetadataBlob {
    crate fn new(metadata_ref: MetadataRef) -> MetadataBlob {
        MetadataBlob(metadata_ref)
    }

    crate fn is_compatible(&self) -> bool {
        self.raw_bytes().starts_with(METADATA_HEADER)
    }

    crate fn get_rustc_version(&self) -> String {
        Lazy::<String>::from_position(NonZeroUsize::new(METADATA_HEADER.len() + 4).unwrap())
            .decode(self)
    }

    crate fn get_root(&self) -> CrateRoot<'tcx> {
        let slice = self.raw_bytes();
        let offset = METADATA_HEADER.len();
        let pos = (((slice[offset + 0] as u32) << 24)
            | ((slice[offset + 1] as u32) << 16)
            | ((slice[offset + 2] as u32) << 8)
            | ((slice[offset + 3] as u32) << 0)) as usize;
        Lazy::<CrateRoot<'tcx>>::from_position(NonZeroUsize::new(pos).unwrap()).decode(self)
    }

    crate fn list_crate_metadata(&self, out: &mut dyn io::Write) -> io::Result<()> {
        write!(out, "=External Dependencies=\n")?;
        let root = self.get_root();
        for (i, dep) in root.crate_deps.decode(self).enumerate() {
            write!(out, "{} {}{}\n", i + 1, dep.name, dep.extra_filename)?;
        }
        write!(out, "\n")?;
        Ok(())
    }
}

impl EntryKind {
    fn def_kind(&self) -> DefKind {
        match *self {
            EntryKind::AnonConst(..) => DefKind::AnonConst,
            EntryKind::Const(..) => DefKind::Const,
            EntryKind::AssocConst(..) => DefKind::AssocConst,
            EntryKind::ImmStatic
            | EntryKind::MutStatic
            | EntryKind::ForeignImmStatic
            | EntryKind::ForeignMutStatic => DefKind::Static,
            EntryKind::Struct(_, _) => DefKind::Struct,
            EntryKind::Union(_, _) => DefKind::Union,
            EntryKind::Fn(_) | EntryKind::ForeignFn(_) => DefKind::Fn,
            EntryKind::AssocFn(_) => DefKind::AssocFn,
            EntryKind::Type => DefKind::TyAlias,
            EntryKind::TypeParam => DefKind::TyParam,
            EntryKind::ConstParam => DefKind::ConstParam,
            EntryKind::OpaqueTy => DefKind::OpaqueTy,
            EntryKind::AssocType(_) => DefKind::AssocTy,
            EntryKind::Mod(_) => DefKind::Mod,
            EntryKind::Variant(_) => DefKind::Variant,
            EntryKind::Trait(_) => DefKind::Trait,
            EntryKind::TraitAlias => DefKind::TraitAlias,
            EntryKind::Enum(..) => DefKind::Enum,
            EntryKind::MacroDef(_) => DefKind::Macro(MacroKind::Bang),
            EntryKind::ForeignType => DefKind::ForeignTy,
            EntryKind::Impl(_) => DefKind::Impl,
            EntryKind::Closure => DefKind::Closure,
            EntryKind::ForeignMod => DefKind::ForeignMod,
            EntryKind::GlobalAsm => DefKind::GlobalAsm,
            EntryKind::Field => DefKind::Field,
            EntryKind::Generator(_) => DefKind::Generator,
        }
    }
}

impl CrateRoot<'_> {
    crate fn is_proc_macro_crate(&self) -> bool {
        self.proc_macro_data.is_some()
    }

    crate fn name(&self) -> Symbol {
        self.name
    }

    crate fn disambiguator(&self) -> CrateDisambiguator {
        self.disambiguator
    }

    crate fn hash(&self) -> Svh {
        self.hash
    }

    crate fn triple(&self) -> &TargetTriple {
        &self.triple
    }

    crate fn decode_crate_deps(
        &self,
        metadata: &'a MetadataBlob,
    ) -> impl ExactSizeIterator<Item = CrateDep> + Captures<'a> {
        self.crate_deps.decode(metadata)
    }
}

impl<'a, 'tcx> CrateMetadataRef<'a> {
    fn is_proc_macro(&self, id: DefIndex) -> bool {
        self.root.proc_macro_data.and_then(|data| data.decode(self).find(|x| *x == id)).is_some()
    }

    fn maybe_kind(&self, item_id: DefIndex) -> Option<EntryKind> {
        self.root.tables.kind.get(self, item_id).map(|k| k.decode(self))
    }

    fn kind(&self, item_id: DefIndex) -> EntryKind {
        assert!(!self.is_proc_macro(item_id));
        self.maybe_kind(item_id).unwrap_or_else(|| {
            bug!(
                "CrateMetadata::kind({:?}): id not found, in crate {:?} with number {}",
                item_id,
                self.root.name,
                self.cnum,
            )
        })
    }

    fn raw_proc_macro(&self, id: DefIndex) -> &ProcMacro {
        // DefIndex's in root.proc_macro_data have a one-to-one correspondence
        // with items in 'raw_proc_macros'.
        let pos = self.root.proc_macro_data.unwrap().decode(self).position(|i| i == id).unwrap();
        &self.raw_proc_macros.unwrap()[pos]
    }

    fn item_ident(&self, item_index: DefIndex, sess: &Session) -> Ident {
        if !self.is_proc_macro(item_index) {
            let name = self
                .def_key(item_index)
                .disambiguated_data
                .data
                .get_opt_name()
                .expect("no name in item_ident");
            let span = self
                .root
                .tables
                .ident_span
                .get(self, item_index)
                .map(|data| data.decode((self, sess)))
                .unwrap_or_else(|| panic!("Missing ident span for {:?} ({:?})", name, item_index));
            Ident::new(name, span)
        } else {
            Ident::new(
                Symbol::intern(self.raw_proc_macro(item_index).name()),
                self.get_span(item_index, sess),
            )
        }
    }

    fn def_kind(&self, index: DefIndex) -> DefKind {
        if !self.is_proc_macro(index) {
            self.kind(index).def_kind()
        } else {
            DefKind::Macro(macro_kind(self.raw_proc_macro(index)))
        }
    }

    fn get_span(&self, index: DefIndex, sess: &Session) -> Span {
        self.root.tables.span.get(self, index).unwrap().decode((self, sess))
    }

    fn load_proc_macro(&self, id: DefIndex, sess: &Session) -> SyntaxExtension {
        let (name, kind, helper_attrs) = match *self.raw_proc_macro(id) {
            ProcMacro::CustomDerive { trait_name, attributes, client } => {
                let helper_attrs =
                    attributes.iter().cloned().map(Symbol::intern).collect::<Vec<_>>();
                (
                    trait_name,
                    SyntaxExtensionKind::Derive(Box::new(ProcMacroDerive { client })),
                    helper_attrs,
                )
            }
            ProcMacro::Attr { name, client } => {
                (name, SyntaxExtensionKind::Attr(Box::new(AttrProcMacro { client })), Vec::new())
            }
            ProcMacro::Bang { name, client } => {
                (name, SyntaxExtensionKind::Bang(Box::new(BangProcMacro { client })), Vec::new())
            }
        };

        SyntaxExtension::new(
            &sess.parse_sess,
            kind,
            self.get_span(id, sess),
            helper_attrs,
            self.root.edition,
            Symbol::intern(name),
            &self.get_item_attrs(id, sess),
        )
    }

    fn get_trait_def(&self, item_id: DefIndex, sess: &Session) -> ty::TraitDef {
        match self.kind(item_id) {
            EntryKind::Trait(data) => {
                let data = data.decode((self, sess));
                ty::TraitDef::new(
                    self.local_def_id(item_id),
                    data.unsafety,
                    data.paren_sugar,
                    data.has_auto_impl,
                    data.is_marker,
                    data.specialization_kind,
                    self.def_path_table.def_path_hash(item_id),
                )
            }
            EntryKind::TraitAlias => ty::TraitDef::new(
                self.local_def_id(item_id),
                hir::Unsafety::Normal,
                false,
                false,
                false,
                ty::trait_def::TraitSpecializationKind::None,
                self.def_path_table.def_path_hash(item_id),
            ),
            _ => bug!("def-index does not refer to trait or trait alias"),
        }
    }

    fn get_variant(
        &self,
        kind: &EntryKind,
        index: DefIndex,
        parent_did: DefId,
        sess: &Session,
    ) -> ty::VariantDef {
        let data = match kind {
            EntryKind::Variant(data) | EntryKind::Struct(data, _) | EntryKind::Union(data, _) => {
                data.decode(self)
            }
            _ => bug!(),
        };

        let adt_kind = match kind {
            EntryKind::Variant(_) => ty::AdtKind::Enum,
            EntryKind::Struct(..) => ty::AdtKind::Struct,
            EntryKind::Union(..) => ty::AdtKind::Union,
            _ => bug!(),
        };

        let variant_did =
            if adt_kind == ty::AdtKind::Enum { Some(self.local_def_id(index)) } else { None };
        let ctor_did = data.ctor.map(|index| self.local_def_id(index));

        ty::VariantDef::new(
            self.item_ident(index, sess),
            variant_did,
            ctor_did,
            data.discr,
            self.root
                .tables
                .children
                .get(self, index)
                .unwrap_or(Lazy::empty())
                .decode(self)
                .map(|index| ty::FieldDef {
                    did: self.local_def_id(index),
                    ident: self.item_ident(index, sess),
                    vis: self.get_visibility(index),
                })
                .collect(),
            data.ctor_kind,
            adt_kind,
            parent_did,
            false,
            data.is_non_exhaustive,
        )
    }

    fn get_adt_def(&self, item_id: DefIndex, tcx: TyCtxt<'tcx>) -> &'tcx ty::AdtDef {
        let kind = self.kind(item_id);
        let did = self.local_def_id(item_id);

        let (adt_kind, repr) = match kind {
            EntryKind::Enum(repr) => (ty::AdtKind::Enum, repr),
            EntryKind::Struct(_, repr) => (ty::AdtKind::Struct, repr),
            EntryKind::Union(_, repr) => (ty::AdtKind::Union, repr),
            _ => bug!("get_adt_def called on a non-ADT {:?}", did),
        };

        let variants = if let ty::AdtKind::Enum = adt_kind {
            self.root
                .tables
                .children
                .get(self, item_id)
                .unwrap_or(Lazy::empty())
                .decode(self)
                .map(|index| self.get_variant(&self.kind(index), index, did, tcx.sess))
                .collect()
        } else {
            std::iter::once(self.get_variant(&kind, item_id, did, tcx.sess)).collect()
        };

        tcx.alloc_adt_def(did, adt_kind, variants, repr)
    }

    fn get_explicit_predicates(
        &self,
        item_id: DefIndex,
        tcx: TyCtxt<'tcx>,
    ) -> ty::GenericPredicates<'tcx> {
        self.root.tables.explicit_predicates.get(self, item_id).unwrap().decode((self, tcx))
    }

    fn get_inferred_outlives(
        &self,
        item_id: DefIndex,
        tcx: TyCtxt<'tcx>,
    ) -> &'tcx [(ty::Predicate<'tcx>, Span)] {
        self.root
            .tables
            .inferred_outlives
            .get(self, item_id)
            .map(|predicates| predicates.decode((self, tcx)))
            .unwrap_or_default()
    }

    fn get_super_predicates(
        &self,
        item_id: DefIndex,
        tcx: TyCtxt<'tcx>,
    ) -> ty::GenericPredicates<'tcx> {
        self.root.tables.super_predicates.get(self, item_id).unwrap().decode((self, tcx))
    }

    fn get_generics(&self, item_id: DefIndex, sess: &Session) -> ty::Generics {
        self.root.tables.generics.get(self, item_id).unwrap().decode((self, sess))
    }

    fn get_type(&self, id: DefIndex, tcx: TyCtxt<'tcx>) -> Ty<'tcx> {
        self.root.tables.ty.get(self, id).unwrap().decode((self, tcx))
    }

    fn get_stability(&self, id: DefIndex) -> Option<attr::Stability> {
        match self.is_proc_macro(id) {
            true => self.root.proc_macro_stability,
            false => self.root.tables.stability.get(self, id).map(|stab| stab.decode(self)),
        }
    }

    fn get_const_stability(&self, id: DefIndex) -> Option<attr::ConstStability> {
        self.root.tables.const_stability.get(self, id).map(|stab| stab.decode(self))
    }

    fn get_deprecation(&self, id: DefIndex) -> Option<attr::Deprecation> {
        self.root
            .tables
            .deprecation
            .get(self, id)
            .filter(|_| !self.is_proc_macro(id))
            .map(|depr| depr.decode(self))
    }

    fn get_visibility(&self, id: DefIndex) -> ty::Visibility {
        match self.is_proc_macro(id) {
            true => ty::Visibility::Public,
            false => self.root.tables.visibility.get(self, id).unwrap().decode(self),
        }
    }

    fn get_impl_data(&self, id: DefIndex) -> ImplData {
        match self.kind(id) {
            EntryKind::Impl(data) => data.decode(self),
            _ => bug!(),
        }
    }

    fn get_parent_impl(&self, id: DefIndex) -> Option<DefId> {
        self.get_impl_data(id).parent_impl
    }

    fn get_impl_polarity(&self, id: DefIndex) -> ty::ImplPolarity {
        self.get_impl_data(id).polarity
    }

    fn get_impl_defaultness(&self, id: DefIndex) -> hir::Defaultness {
        self.get_impl_data(id).defaultness
    }

    fn get_coerce_unsized_info(&self, id: DefIndex) -> Option<ty::adjustment::CoerceUnsizedInfo> {
        self.get_impl_data(id).coerce_unsized_info
    }

    fn get_impl_trait(&self, id: DefIndex, tcx: TyCtxt<'tcx>) -> Option<ty::TraitRef<'tcx>> {
        self.root.tables.impl_trait_ref.get(self, id).map(|tr| tr.decode((self, tcx)))
    }

    /// Iterates over all the stability attributes in the given crate.
    fn get_lib_features(&self, tcx: TyCtxt<'tcx>) -> &'tcx [(Symbol, Option<Symbol>)] {
        // FIXME: For a proc macro crate, not sure whether we should return the "host"
        // features or an empty Vec. Both don't cause ICEs.
        tcx.arena.alloc_from_iter(self.root.lib_features.decode(self))
    }

    /// Iterates over the language items in the given crate.
    fn get_lang_items(&self, tcx: TyCtxt<'tcx>) -> &'tcx [(DefId, usize)] {
        if self.root.is_proc_macro_crate() {
            // Proc macro crates do not export any lang-items to the target.
            &[]
        } else {
            tcx.arena.alloc_from_iter(
                self.root
                    .lang_items
                    .decode(self)
                    .map(|(def_index, index)| (self.local_def_id(def_index), index)),
            )
        }
    }

    /// Iterates over the diagnostic items in the given crate.
    fn get_diagnostic_items(&self) -> FxHashMap<Symbol, DefId> {
        if self.root.is_proc_macro_crate() {
            // Proc macro crates do not export any diagnostic-items to the target.
            Default::default()
        } else {
            self.root
                .diagnostic_items
                .decode(self)
                .map(|(name, def_index)| (name, self.local_def_id(def_index)))
                .collect()
        }
    }

    /// Iterates over each child of the given item.
    fn each_child_of_item<F>(&self, id: DefIndex, mut callback: F, sess: &Session)
    where
        F: FnMut(Export<hir::HirId>),
    {
        if let Some(proc_macros_ids) = self.root.proc_macro_data.map(|d| d.decode(self)) {
            /* If we are loading as a proc macro, we want to return the view of this crate
             * as a proc macro crate.
             */
            if id == CRATE_DEF_INDEX {
                for def_index in proc_macros_ids {
                    let raw_macro = self.raw_proc_macro(def_index);
                    let res = Res::Def(
                        DefKind::Macro(macro_kind(raw_macro)),
                        self.local_def_id(def_index),
                    );
                    let ident = self.item_ident(def_index, sess);
                    callback(Export {
                        ident,
                        res,
                        vis: ty::Visibility::Public,
                        span: self.get_span(def_index, sess),
                    });
                }
            }
            return;
        }

        // Find the item.
        let kind = match self.maybe_kind(id) {
            None => return,
            Some(kind) => kind,
        };

        // Iterate over all children.
        let macros_only = self.dep_kind.lock().macros_only();
        let children = self.root.tables.children.get(self, id).unwrap_or(Lazy::empty());
        for child_index in children.decode((self, sess)) {
            if macros_only {
                continue;
            }

            // Get the item.
            if let Some(child_kind) = self.maybe_kind(child_index) {
                match child_kind {
                    EntryKind::MacroDef(..) => {}
                    _ if macros_only => continue,
                    _ => {}
                }

                // Hand off the item to the callback.
                match child_kind {
                    // FIXME(eddyb) Don't encode these in children.
                    EntryKind::ForeignMod => {
                        let child_children = self
                            .root
                            .tables
                            .children
                            .get(self, child_index)
                            .unwrap_or(Lazy::empty());
                        for child_index in child_children.decode((self, sess)) {
                            let kind = self.def_kind(child_index);
                            callback(Export {
                                res: Res::Def(kind, self.local_def_id(child_index)),
                                ident: self.item_ident(child_index, sess),
                                vis: self.get_visibility(child_index),
                                span: self
                                    .root
                                    .tables
                                    .span
                                    .get(self, child_index)
                                    .unwrap()
                                    .decode((self, sess)),
                            });
                        }
                        continue;
                    }
                    EntryKind::Impl(_) => continue,

                    _ => {}
                }

                let def_key = self.def_key(child_index);
                let span = self.get_span(child_index, sess);
                if def_key.disambiguated_data.data.get_opt_name().is_some() {
                    let kind = self.def_kind(child_index);
                    let ident = self.item_ident(child_index, sess);
                    let vis = self.get_visibility(child_index);
                    let def_id = self.local_def_id(child_index);
                    let res = Res::Def(kind, def_id);
                    callback(Export { res, ident, vis, span });
                    // For non-re-export structs and variants add their constructors to children.
                    // Re-export lists automatically contain constructors when necessary.
                    match kind {
                        DefKind::Struct => {
                            if let Some(ctor_def_id) = self.get_ctor_def_id(child_index) {
                                let ctor_kind = self.get_ctor_kind(child_index);
                                let ctor_res =
                                    Res::Def(DefKind::Ctor(CtorOf::Struct, ctor_kind), ctor_def_id);
                                let vis = self.get_visibility(ctor_def_id.index);
                                callback(Export { res: ctor_res, vis, ident, span });
                            }
                        }
                        DefKind::Variant => {
                            // Braced variants, unlike structs, generate unusable names in
                            // value namespace, they are reserved for possible future use.
                            // It's ok to use the variant's id as a ctor id since an
                            // error will be reported on any use of such resolution anyway.
                            let ctor_def_id = self.get_ctor_def_id(child_index).unwrap_or(def_id);
                            let ctor_kind = self.get_ctor_kind(child_index);
                            let ctor_res =
                                Res::Def(DefKind::Ctor(CtorOf::Variant, ctor_kind), ctor_def_id);
                            let mut vis = self.get_visibility(ctor_def_id.index);
                            if ctor_def_id == def_id && vis == ty::Visibility::Public {
                                // For non-exhaustive variants lower the constructor visibility to
                                // within the crate. We only need this for fictive constructors,
                                // for other constructors correct visibilities
                                // were already encoded in metadata.
                                let attrs = self.get_item_attrs(def_id.index, sess);
                                if attr::contains_name(&attrs, sym::non_exhaustive) {
                                    let crate_def_id = self.local_def_id(CRATE_DEF_INDEX);
                                    vis = ty::Visibility::Restricted(crate_def_id);
                                }
                            }
                            callback(Export { res: ctor_res, ident, vis, span });
                        }
                        _ => {}
                    }
                }
            }
        }

        if let EntryKind::Mod(data) = kind {
            for exp in data.decode((self, sess)).reexports.decode((self, sess)) {
                match exp.res {
                    Res::Def(DefKind::Macro(..), _) => {}
                    _ if macros_only => continue,
                    _ => {}
                }
                callback(exp);
            }
        }
    }

    fn is_item_mir_available(&self, id: DefIndex) -> bool {
        !self.is_proc_macro(id) && self.root.tables.mir.get(self, id).is_some()
    }

    fn module_expansion(&self, id: DefIndex, sess: &Session) -> ExpnId {
        if let EntryKind::Mod(m) = self.kind(id) {
            m.decode((self, sess)).expansion
        } else {
            panic!("Expected module, found {:?}", self.local_def_id(id))
        }
    }

    fn get_optimized_mir(&self, tcx: TyCtxt<'tcx>, id: DefIndex) -> Body<'tcx> {
        self.root
            .tables
            .mir
            .get(self, id)
            .filter(|_| !self.is_proc_macro(id))
            .unwrap_or_else(|| {
                bug!("get_optimized_mir: missing MIR for `{:?}`", self.local_def_id(id))
            })
            .decode((self, tcx))
    }

    fn get_unused_generic_params(&self, id: DefIndex) -> FiniteBitSet<u32> {
        self.root
            .tables
            .unused_generic_params
            .get(self, id)
            .filter(|_| !self.is_proc_macro(id))
            .map(|params| params.decode(self))
            .unwrap_or_default()
    }

    fn get_promoted_mir(&self, tcx: TyCtxt<'tcx>, id: DefIndex) -> IndexVec<Promoted, Body<'tcx>> {
        self.root
            .tables
            .promoted_mir
            .get(self, id)
            .filter(|_| !self.is_proc_macro(id))
            .unwrap_or_else(|| {
                bug!("get_promoted_mir: missing MIR for `{:?}`", self.local_def_id(id))
            })
            .decode((self, tcx))
    }

    fn mir_const_qualif(&self, id: DefIndex) -> mir::ConstQualifs {
        match self.kind(id) {
            EntryKind::AnonConst(qualif, _)
            | EntryKind::Const(qualif, _)
            | EntryKind::AssocConst(
                AssocContainer::ImplDefault
                | AssocContainer::ImplFinal
                | AssocContainer::TraitWithDefault,
                qualif,
                _,
            ) => qualif,
            _ => bug!("mir_const_qualif: unexpected kind"),
        }
    }

    fn get_associated_item(&self, id: DefIndex, sess: &Session) -> ty::AssocItem {
        let def_key = self.def_key(id);
        let parent = self.local_def_id(def_key.parent.unwrap());
        let ident = self.item_ident(id, sess);

        let (kind, container, has_self) = match self.kind(id) {
            EntryKind::AssocConst(container, _, _) => (ty::AssocKind::Const, container, false),
            EntryKind::AssocFn(data) => {
                let data = data.decode(self);
                (ty::AssocKind::Fn, data.container, data.has_self)
            }
            EntryKind::AssocType(container) => (ty::AssocKind::Type, container, false),
            _ => bug!("cannot get associated-item of `{:?}`", def_key),
        };

        ty::AssocItem {
            ident,
            kind,
            vis: self.get_visibility(id),
            defaultness: container.defaultness(),
            def_id: self.local_def_id(id),
            container: container.with_def_id(parent),
            fn_has_self_parameter: has_self,
        }
    }

    fn get_item_variances(&self, id: DefIndex) -> Vec<ty::Variance> {
        self.root.tables.variances.get(self, id).unwrap_or(Lazy::empty()).decode(self).collect()
    }

    fn get_ctor_kind(&self, node_id: DefIndex) -> CtorKind {
        match self.kind(node_id) {
            EntryKind::Struct(data, _) | EntryKind::Union(data, _) | EntryKind::Variant(data) => {
                data.decode(self).ctor_kind
            }
            _ => CtorKind::Fictive,
        }
    }

    fn get_ctor_def_id(&self, node_id: DefIndex) -> Option<DefId> {
        match self.kind(node_id) {
            EntryKind::Struct(data, _) => {
                data.decode(self).ctor.map(|index| self.local_def_id(index))
            }
            EntryKind::Variant(data) => {
                data.decode(self).ctor.map(|index| self.local_def_id(index))
            }
            _ => None,
        }
    }

    fn get_item_attrs(&self, node_id: DefIndex, sess: &Session) -> Vec<ast::Attribute> {
        // The attributes for a tuple struct/variant are attached to the definition, not the ctor;
        // we assume that someone passing in a tuple struct ctor is actually wanting to
        // look at the definition
        let def_key = self.def_key(node_id);
        let item_id = if def_key.disambiguated_data.data == DefPathData::Ctor {
            def_key.parent.unwrap()
        } else {
            node_id
        };

        self.root
            .tables
            .attributes
            .get(self, item_id)
            .unwrap_or(Lazy::empty())
            .decode((self, sess))
            .collect::<Vec<_>>()
    }

    fn get_struct_field_names(&self, id: DefIndex, sess: &Session) -> Vec<Spanned<Symbol>> {
        self.root
            .tables
            .children
            .get(self, id)
            .unwrap_or(Lazy::empty())
            .decode(self)
            .map(|index| respan(self.get_span(index, sess), self.item_ident(index, sess).name))
            .collect()
    }

    fn get_inherent_implementations_for_type(
        &self,
        tcx: TyCtxt<'tcx>,
        id: DefIndex,
    ) -> &'tcx [DefId] {
        tcx.arena.alloc_from_iter(
            self.root
                .tables
                .inherent_impls
                .get(self, id)
                .unwrap_or(Lazy::empty())
                .decode(self)
                .map(|index| self.local_def_id(index)),
        )
    }

    fn get_implementations_for_trait(
        &self,
        tcx: TyCtxt<'tcx>,
        filter: Option<DefId>,
    ) -> &'tcx [DefId] {
        if self.root.is_proc_macro_crate() {
            // proc-macro crates export no trait impls.
            return &[];
        }

        // Do a reverse lookup beforehand to avoid touching the crate_num
        // hash map in the loop below.
        let filter = match filter.map(|def_id| self.reverse_translate_def_id(def_id)) {
            Some(Some(def_id)) => Some((def_id.krate.as_u32(), def_id.index)),
            Some(None) => return &[],
            None => None,
        };

        if let Some(filter) = filter {
            if let Some(impls) = self.trait_impls.get(&filter) {
                tcx.arena.alloc_from_iter(impls.decode(self).map(|idx| self.local_def_id(idx)))
            } else {
                &[]
            }
        } else {
            tcx.arena.alloc_from_iter(
                self.trait_impls
                    .values()
                    .flat_map(|impls| impls.decode(self).map(|idx| self.local_def_id(idx))),
            )
        }
    }

    fn get_trait_of_item(&self, id: DefIndex) -> Option<DefId> {
        let def_key = self.def_key(id);
        match def_key.disambiguated_data.data {
            DefPathData::TypeNs(..) | DefPathData::ValueNs(..) => (),
            // Not an associated item
            _ => return None,
        }
        def_key.parent.and_then(|parent_index| match self.kind(parent_index) {
            EntryKind::Trait(_) | EntryKind::TraitAlias => Some(self.local_def_id(parent_index)),
            _ => None,
        })
    }

    fn get_native_libraries(&self, sess: &Session) -> Vec<NativeLib> {
        if self.root.is_proc_macro_crate() {
            // Proc macro crates do not have any *target* native libraries.
            vec![]
        } else {
            self.root.native_libraries.decode((self, sess)).collect()
        }
    }

    fn get_foreign_modules(&self, tcx: TyCtxt<'tcx>) -> &'tcx [ForeignModule] {
        if self.root.is_proc_macro_crate() {
            // Proc macro crates do not have any *target* foreign modules.
            &[]
        } else {
            tcx.arena.alloc_from_iter(self.root.foreign_modules.decode((self, tcx.sess)))
        }
    }

    fn get_dylib_dependency_formats(
        &self,
        tcx: TyCtxt<'tcx>,
    ) -> &'tcx [(CrateNum, LinkagePreference)] {
        tcx.arena.alloc_from_iter(
            self.root.dylib_dependency_formats.decode(self).enumerate().flat_map(|(i, link)| {
                let cnum = CrateNum::new(i + 1);
                link.map(|link| (self.cnum_map[cnum], link))
            }),
        )
    }

    fn get_missing_lang_items(&self, tcx: TyCtxt<'tcx>) -> &'tcx [lang_items::LangItem] {
        if self.root.is_proc_macro_crate() {
            // Proc macro crates do not depend on any target weak lang-items.
            &[]
        } else {
            tcx.arena.alloc_from_iter(self.root.lang_items_missing.decode(self))
        }
    }

    fn get_fn_param_names(&self, tcx: TyCtxt<'tcx>, id: DefIndex) -> &'tcx [Ident] {
        let param_names = match self.kind(id) {
            EntryKind::Fn(data) | EntryKind::ForeignFn(data) => data.decode(self).param_names,
            EntryKind::AssocFn(data) => data.decode(self).fn_data.param_names,
            _ => Lazy::empty(),
        };
        tcx.arena.alloc_from_iter(param_names.decode((self, tcx)))
    }

    fn exported_symbols(
        &self,
        tcx: TyCtxt<'tcx>,
    ) -> &'tcx [(ExportedSymbol<'tcx>, SymbolExportLevel)] {
        if self.root.is_proc_macro_crate() {
            // If this crate is a custom derive crate, then we're not even going to
            // link those in so we skip those crates.
            &[]
        } else {
            tcx.arena.alloc_from_iter(self.root.exported_symbols.decode((self, tcx)))
        }
    }

    fn get_rendered_const(&self, id: DefIndex) -> String {
        match self.kind(id) {
            EntryKind::AnonConst(_, data)
            | EntryKind::Const(_, data)
            | EntryKind::AssocConst(_, _, data) => data.decode(self).0,
            _ => bug!(),
        }
    }

    fn get_macro(&self, id: DefIndex, sess: &Session) -> MacroDef {
        match self.kind(id) {
            EntryKind::MacroDef(macro_def) => macro_def.decode((self, sess)),
            _ => bug!(),
        }
    }

    // This replicates some of the logic of the crate-local `is_const_fn_raw` query, because we
    // don't serialize constness for tuple variant and tuple struct constructors.
    fn is_const_fn_raw(&self, id: DefIndex) -> bool {
        let constness = match self.kind(id) {
            EntryKind::AssocFn(data) => data.decode(self).fn_data.constness,
            EntryKind::Fn(data) => data.decode(self).constness,
            EntryKind::ForeignFn(data) => data.decode(self).constness,
            EntryKind::Variant(..) | EntryKind::Struct(..) => hir::Constness::Const,
            _ => hir::Constness::NotConst,
        };
        constness == hir::Constness::Const
    }

    fn asyncness(&self, id: DefIndex) -> hir::IsAsync {
        match self.kind(id) {
            EntryKind::Fn(data) => data.decode(self).asyncness,
            EntryKind::AssocFn(data) => data.decode(self).fn_data.asyncness,
            EntryKind::ForeignFn(data) => data.decode(self).asyncness,
            _ => bug!("asyncness: expected function kind"),
        }
    }

    fn is_foreign_item(&self, id: DefIndex) -> bool {
        match self.kind(id) {
            EntryKind::ForeignImmStatic | EntryKind::ForeignMutStatic | EntryKind::ForeignFn(_) => {
                true
            }
            _ => false,
        }
    }

    fn static_mutability(&self, id: DefIndex) -> Option<hir::Mutability> {
        match self.kind(id) {
            EntryKind::ImmStatic | EntryKind::ForeignImmStatic => Some(hir::Mutability::Not),
            EntryKind::MutStatic | EntryKind::ForeignMutStatic => Some(hir::Mutability::Mut),
            _ => None,
        }
    }

    fn generator_kind(&self, id: DefIndex) -> Option<hir::GeneratorKind> {
        match self.kind(id) {
            EntryKind::Generator(data) => Some(data),
            _ => None,
        }
    }

    fn fn_sig(&self, id: DefIndex, tcx: TyCtxt<'tcx>) -> ty::PolyFnSig<'tcx> {
        self.root.tables.fn_sig.get(self, id).unwrap().decode((self, tcx))
    }

    #[inline]
    fn def_key(&self, index: DefIndex) -> DefKey {
        let mut key = self.def_path_table.def_key(index);
        if self.is_proc_macro(index) {
            let name = self.raw_proc_macro(index).name();
            key.disambiguated_data.data = DefPathData::MacroNs(Symbol::intern(name));
        }
        key
    }

    // Returns the path leading to the thing with this `id`.
    fn def_path(&self, id: DefIndex) -> DefPath {
        debug!("def_path(cnum={:?}, id={:?})", self.cnum, id);
        DefPath::make(self.cnum, id, |parent| self.def_key(parent))
    }

    /// Imports the source_map from an external crate into the source_map of the crate
    /// currently being compiled (the "local crate").
    ///
    /// The import algorithm works analogous to how AST items are inlined from an
    /// external crate's metadata:
    /// For every SourceFile in the external source_map an 'inline' copy is created in the
    /// local source_map. The correspondence relation between external and local
    /// SourceFiles is recorded in the `ImportedSourceFile` objects returned from this
    /// function. When an item from an external crate is later inlined into this
    /// crate, this correspondence information is used to translate the span
    /// information of the inlined item so that it refers the correct positions in
    /// the local source_map (see `<decoder::DecodeContext as SpecializedDecoder<Span>>`).
    ///
    /// The import algorithm in the function below will reuse SourceFiles already
    /// existing in the local source_map. For example, even if the SourceFile of some
    /// source file of libstd gets imported many times, there will only ever be
    /// one SourceFile object for the corresponding file in the local source_map.
    ///
    /// Note that imported SourceFiles do not actually contain the source code of the
    /// file they represent, just information about length, line breaks, and
    /// multibyte characters. This information is enough to generate valid debuginfo
    /// for items inlined from other crates.
    ///
    /// Proc macro crates don't currently export spans, so this function does not have
    /// to work for them.
    fn imported_source_files(&self, sess: &Session) -> &'a [ImportedSourceFile] {
        // Translate the virtual `/rustc/$hash` prefix back to a real directory
        // that should hold actual sources, where possible.
        //
        // NOTE: if you update this, you might need to also update bootstrap's code for generating
        // the `rust-src` component in `Src::run` in `src/bootstrap/dist.rs`.
        let virtual_rust_source_base_dir = option_env!("CFG_VIRTUAL_RUST_SOURCE_BASE_DIR")
            .map(Path::new)
            .filter(|_| {
                // Only spend time on further checks if we have what to translate *to*.
                sess.real_rust_source_base_dir.is_some()
            })
            .filter(|virtual_dir| {
                // Don't translate away `/rustc/$hash` if we're still remapping to it,
                // since that means we're still building `std`/`rustc` that need it,
                // and we don't want the real path to leak into codegen/debuginfo.
                !sess.opts.remap_path_prefix.iter().any(|(_from, to)| to == virtual_dir)
            });
        let try_to_translate_virtual_to_real = |name: &mut rustc_span::FileName| {
            debug!(
                "try_to_translate_virtual_to_real(name={:?}): \
                 virtual_rust_source_base_dir={:?}, real_rust_source_base_dir={:?}",
                name, virtual_rust_source_base_dir, sess.real_rust_source_base_dir,
            );

            if let Some(virtual_dir) = virtual_rust_source_base_dir {
                if let Some(real_dir) = &sess.real_rust_source_base_dir {
                    if let rustc_span::FileName::Real(old_name) = name {
                        if let rustc_span::RealFileName::Named(one_path) = old_name {
                            if let Ok(rest) = one_path.strip_prefix(virtual_dir) {
                                let virtual_name = one_path.clone();

                                // The std library crates are in
                                // `$sysroot/lib/rustlib/src/rust/library`, whereas other crates
                                // may be in `$sysroot/lib/rustlib/src/rust/` directly. So we
                                // detect crates from the std libs and handle them specially.
                                const STD_LIBS: &[&str] = &[
                                    "core",
                                    "alloc",
                                    "std",
                                    "test",
                                    "term",
                                    "unwind",
                                    "proc_macro",
                                    "panic_abort",
                                    "panic_unwind",
                                    "profiler_builtins",
                                    "rtstartup",
                                    "rustc-std-workspace-core",
                                    "rustc-std-workspace-alloc",
                                    "rustc-std-workspace-std",
                                    "backtrace",
                                ];
                                let is_std_lib = STD_LIBS.iter().any(|l| rest.starts_with(l));

                                let new_path = if is_std_lib {
                                    real_dir.join("library").join(rest)
                                } else {
                                    real_dir.join(rest)
                                };

                                debug!(
                                    "try_to_translate_virtual_to_real: `{}` -> `{}`",
                                    virtual_name.display(),
                                    new_path.display(),
                                );
                                let new_name = rustc_span::RealFileName::Devirtualized {
                                    local_path: new_path,
                                    virtual_name,
                                };
                                *old_name = new_name;
                            }
                        }
                    }
                }
            }
        };

        self.cdata.source_map_import_info.get_or_init(|| {
            let external_source_map = self.root.source_map.decode(self);

            external_source_map
                .map(|source_file_to_import| {
                    // We can't reuse an existing SourceFile, so allocate a new one
                    // containing the information we need.
                    let rustc_span::SourceFile {
                        mut name,
                        name_was_remapped,
                        src_hash,
                        start_pos,
                        end_pos,
                        mut lines,
                        mut multibyte_chars,
                        mut non_narrow_chars,
                        mut normalized_pos,
                        name_hash,
                        ..
                    } = source_file_to_import;

                    // If this file's path has been remapped to `/rustc/$hash`,
                    // we might be able to reverse that (also see comments above,
                    // on `try_to_translate_virtual_to_real`).
                    // FIXME(eddyb) we could check `name_was_remapped` here,
                    // but in practice it seems to be always `false`.
                    try_to_translate_virtual_to_real(&mut name);

                    let source_length = (end_pos - start_pos).to_usize();

                    // Translate line-start positions and multibyte character
                    // position into frame of reference local to file.
                    // `SourceMap::new_imported_source_file()` will then translate those
                    // coordinates to their new global frame of reference when the
                    // offset of the SourceFile is known.
                    for pos in &mut lines {
                        *pos = *pos - start_pos;
                    }
                    for mbc in &mut multibyte_chars {
                        mbc.pos = mbc.pos - start_pos;
                    }
                    for swc in &mut non_narrow_chars {
                        *swc = *swc - start_pos;
                    }
                    for np in &mut normalized_pos {
                        np.pos = np.pos - start_pos;
                    }

                    let local_version = sess.source_map().new_imported_source_file(
                        name,
                        name_was_remapped,
                        src_hash,
                        name_hash,
                        source_length,
                        self.cnum,
                        lines,
                        multibyte_chars,
                        non_narrow_chars,
                        normalized_pos,
                        start_pos,
                        end_pos,
                    );
                    debug!(
                        "CrateMetaData::imported_source_files alloc \
                         source_file {:?} original (start_pos {:?} end_pos {:?}) \
                         translated (start_pos {:?} end_pos {:?})",
                        local_version.name,
                        start_pos,
                        end_pos,
                        local_version.start_pos,
                        local_version.end_pos
                    );

                    ImportedSourceFile {
                        original_start_pos: start_pos,
                        original_end_pos: end_pos,
                        translated_source_file: local_version,
                    }
                })
                .collect()
        })
    }
}

impl CrateMetadata {
    crate fn new(
        sess: &Session,
        blob: MetadataBlob,
        root: CrateRoot<'static>,
        raw_proc_macros: Option<&'static [ProcMacro]>,
        cnum: CrateNum,
        cnum_map: CrateNumMap,
        dep_kind: CrateDepKind,
        source: CrateSource,
        private_dep: bool,
        host_hash: Option<Svh>,
    ) -> CrateMetadata {
        let def_path_table = record_time(&sess.perf_stats.decode_def_path_tables_time, || {
            root.def_path_table.decode((&blob, sess))
        });
        let trait_impls = root
            .impls
            .decode((&blob, sess))
            .map(|trait_impls| (trait_impls.trait_id, trait_impls.impls))
            .collect();
        let alloc_decoding_state =
            AllocDecodingState::new(root.interpret_alloc_index.decode(&blob).collect());
        let dependencies = Lock::new(cnum_map.iter().cloned().collect());
        CrateMetadata {
            blob,
            root,
            def_path_table,
            trait_impls,
            raw_proc_macros,
            source_map_import_info: OnceCell::new(),
            alloc_decoding_state,
            dep_node_index: AtomicCell::new(DepNodeIndex::INVALID),
            cnum,
            cnum_map,
            dependencies,
            dep_kind: Lock::new(dep_kind),
            source,
            private_dep,
            host_hash,
            extern_crate: Lock::new(None),
            hygiene_context: Default::default(),
        }
    }

    crate fn dependencies(&self) -> LockGuard<'_, Vec<CrateNum>> {
        self.dependencies.borrow()
    }

    crate fn add_dependency(&self, cnum: CrateNum) {
        self.dependencies.borrow_mut().push(cnum);
    }

    crate fn update_extern_crate(&self, new_extern_crate: ExternCrate) -> bool {
        let mut extern_crate = self.extern_crate.borrow_mut();
        let update = Some(new_extern_crate.rank()) > extern_crate.as_ref().map(ExternCrate::rank);
        if update {
            *extern_crate = Some(new_extern_crate);
        }
        update
    }

    crate fn source(&self) -> &CrateSource {
        &self.source
    }

    crate fn dep_kind(&self) -> CrateDepKind {
        *self.dep_kind.lock()
    }

    crate fn update_dep_kind(&self, f: impl FnOnce(CrateDepKind) -> CrateDepKind) {
        self.dep_kind.with_lock(|dep_kind| *dep_kind = f(*dep_kind))
    }

    crate fn panic_strategy(&self) -> PanicStrategy {
        self.root.panic_strategy
    }

    crate fn needs_panic_runtime(&self) -> bool {
        self.root.needs_panic_runtime
    }

    crate fn is_panic_runtime(&self) -> bool {
        self.root.panic_runtime
    }

    crate fn is_profiler_runtime(&self) -> bool {
        self.root.profiler_runtime
    }

    crate fn needs_allocator(&self) -> bool {
        self.root.needs_allocator
    }

    crate fn has_global_allocator(&self) -> bool {
        self.root.has_global_allocator
    }

    crate fn has_default_lib_allocator(&self) -> bool {
        self.root.has_default_lib_allocator
    }

    crate fn is_proc_macro_crate(&self) -> bool {
        self.root.is_proc_macro_crate()
    }

    crate fn name(&self) -> Symbol {
        self.root.name
    }

    crate fn disambiguator(&self) -> CrateDisambiguator {
        self.root.disambiguator
    }

    crate fn hash(&self) -> Svh {
        self.root.hash
    }

    fn local_def_id(&self, index: DefIndex) -> DefId {
        DefId { krate: self.cnum, index }
    }

    // Translate a DefId from the current compilation environment to a DefId
    // for an external crate.
    fn reverse_translate_def_id(&self, did: DefId) -> Option<DefId> {
        for (local, &global) in self.cnum_map.iter_enumerated() {
            if global == did.krate {
                return Some(DefId { krate: local, index: did.index });
            }
        }

        None
    }

    #[inline]
    fn def_path_hash(&self, index: DefIndex) -> DefPathHash {
        self.def_path_table.def_path_hash(index)
    }

    /// Get the `DepNodeIndex` corresponding this crate. The result of this
    /// method is cached in the `dep_node_index` field.
    fn get_crate_dep_node_index(&self, tcx: TyCtxt<'tcx>) -> DepNodeIndex {
        let mut dep_node_index = self.dep_node_index.load();

        if unlikely!(dep_node_index == DepNodeIndex::INVALID) {
            // We have not cached the DepNodeIndex for this upstream crate yet,
            // so use the dep-graph to find it out and cache it.
            // Note that multiple threads can enter this block concurrently.
            // That is fine because the DepNodeIndex remains constant
            // throughout the whole compilation session, and multiple stores
            // would always write the same value.

            let def_path_hash = self.def_path_hash(CRATE_DEF_INDEX);
            let dep_node =
                DepNode::from_def_path_hash(def_path_hash, dep_graph::DepKind::CrateMetadata);

            dep_node_index = tcx.dep_graph.dep_node_index_of(&dep_node);
            assert!(dep_node_index != DepNodeIndex::INVALID);
            self.dep_node_index.store(dep_node_index);
        }

        dep_node_index
    }
}

// Cannot be implemented on 'ProcMacro', as libproc_macro
// does not depend on librustc_ast
fn macro_kind(raw: &ProcMacro) -> MacroKind {
    match raw {
        ProcMacro::CustomDerive { .. } => MacroKind::Derive,
        ProcMacro::Attr { .. } => MacroKind::Attr,
        ProcMacro::Bang { .. } => MacroKind::Bang,
    }
}

impl<'a, 'tcx> SpecializedDecoder<SyntaxContext> for DecodeContext<'a, 'tcx> {
    fn specialized_decode(&mut self) -> Result<SyntaxContext, Self::Error> {
        let cdata = self.cdata();
        let sess = self.sess.unwrap();
        let cname = cdata.root.name;
        rustc_span::hygiene::decode_syntax_context(self, &cdata.hygiene_context, |_, id| {
            debug!("SpecializedDecoder<SyntaxContext>: decoding {}", id);
            Ok(cdata
                .root
                .syntax_contexts
                .get(&cdata, id)
                .unwrap_or_else(|| panic!("Missing SyntaxContext {:?} for crate {:?}", id, cname))
                .decode((&cdata, sess)))
        })
    }
}

impl<'a, 'tcx> SpecializedDecoder<ExpnId> for DecodeContext<'a, 'tcx> {
    fn specialized_decode(&mut self) -> Result<ExpnId, Self::Error> {
        let local_cdata = self.cdata();
        let sess = self.sess.unwrap();
        let expn_cnum = Cell::new(None);
        let get_ctxt = |cnum| {
            expn_cnum.set(Some(cnum));
            if cnum == LOCAL_CRATE {
                &local_cdata.hygiene_context
            } else {
                &local_cdata.cstore.get_crate_data(cnum).cdata.hygiene_context
            }
        };

        rustc_span::hygiene::decode_expn_id(
            self,
            ExpnDataDecodeMode::Metadata(get_ctxt),
            |_this, index| {
                let cnum = expn_cnum.get().unwrap();
                // Lookup local `ExpnData`s in our own crate data. Foreign `ExpnData`s
                // are stored in the owning crate, to avoid duplication.
                let crate_data = if cnum == LOCAL_CRATE {
                    local_cdata
                } else {
                    local_cdata.cstore.get_crate_data(cnum)
                };
                Ok(crate_data
                    .root
                    .expn_data
                    .get(&crate_data, index)
                    .unwrap()
                    .decode((&crate_data, sess)))
            },
        )
    }
}
