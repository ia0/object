//! Support for reading Wasm files.
//!
//! Provides `WasmFile` and related types which implement the `Object` trait.
//!
//! Currently implements the minimum required to access DWARF debugging information.
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ops::Range;
use core::{slice, str};
use wasmparser as wp;

use crate::read::{
    self, Architecture, ComdatKind, CompressedData, CompressedFileRange, Error, Export, FileFlags,
    Import, NoDynamicRelocationIterator, Object, ObjectComdat, ObjectKind, ObjectSection,
    ObjectSegment, ObjectSymbol, ObjectSymbolTable, ReadError, ReadRef, Relocation, Result,
    SectionFlags, SectionIndex, SectionKind, SegmentFlags, SymbolFlags, SymbolIndex, SymbolKind,
    SymbolScope, SymbolSection,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum SectionId {
    Custom = 0,
    Type = 1,
    Import = 2,
    Function = 3,
    Table = 4,
    Memory = 5,
    Global = 6,
    Export = 7,
    Start = 8,
    Element = 9,
    Code = 10,
    Data = 11,
    DataCount = 12,
}
// Update this constant when adding new section id:
const MAX_SECTION_ID: usize = SectionId::DataCount as usize;

/// A WebAssembly object file.
#[derive(Debug)]
pub struct WasmFile<'data, R = &'data [u8]> {
    data: &'data [u8],
    // All sections, including custom sections.
    sections: Vec<SectionHeader<'data>>,
    // Indices into `sections` of sections with a non-zero id.
    id_sections: Box<[Option<usize>; MAX_SECTION_ID + 1]>,
    // Whether the file has DWARF information.
    has_debug_symbols: bool,
    // Symbols collected from imports, exports, code and name sections.
    symbols: Vec<WasmSymbolInternal<'data>>,
    // Address of the function body for the entry point.
    entry: u64,
    marker: PhantomData<R>,
}

#[derive(Debug)]
struct SectionHeader<'data> {
    id: SectionId,
    range: Range<usize>,
    name: &'data str,
}

#[derive(Clone)]
enum LocalFunctionKind {
    Unknown,
    Exported { symbol_ids: Vec<u32> },
    Local { symbol_id: u32 },
}

impl<T> ReadError<T> for wasmparser::Result<T> {
    fn read_error(self, error: &'static str) -> Result<T> {
        self.map_err(|_| Error(error))
    }
}

impl<'data, R: ReadRef<'data>> WasmFile<'data, R> {
    /// Parse the raw wasm data.
    pub fn parse(data: R) -> Result<Self> {
        let len = data.len().read_error("Unknown Wasm file size")?;
        let data = data.read_bytes_at(0, len).read_error("Wasm read failed")?;
        let parser = wp::Parser::new(0).parse_all(data);

        let mut file = WasmFile {
            data,
            sections: Vec::new(),
            id_sections: Default::default(),
            has_debug_symbols: false,
            symbols: Vec::new(),
            entry: 0,
            marker: PhantomData,
        };

        let mut main_file_symbol = Some(WasmSymbolInternal {
            name: "",
            address: 0,
            size: 0,
            kind: SymbolKind::File,
            section: SymbolSection::None,
            scope: SymbolScope::Compilation,
        });

        let mut imported_funcs_count = 0;
        let mut local_func_kinds = Vec::new();
        let mut entry_func_id = None;
        let mut code_range_start = 0;
        let mut code_func_index = 0;

        for payload in parser {
            let payload = payload.read_error("Invalid Wasm section header")?;

            match payload {
                wp::Payload::TypeSection(section) => {
                    file.add_section(SectionId::Type, section.range(), "");
                }
                wp::Payload::ImportSection(section) => {
                    file.add_section(SectionId::Import, section.range(), "");
                    let mut last_module_name = None;

                    for import in section {
                        let import = import.read_error("Couldn't read an import item")?;
                        let module_name = import.module;

                        if last_module_name != Some(module_name) {
                            file.symbols.push(WasmSymbolInternal {
                                name: module_name,
                                address: 0,
                                size: 0,
                                kind: SymbolKind::File,
                                section: SymbolSection::None,
                                scope: SymbolScope::Dynamic,
                            });
                            last_module_name = Some(module_name);
                        }

                        let kind = match import.ty {
                            wp::TypeRef::Func(_) => {
                                imported_funcs_count += 1;
                                SymbolKind::Text
                            }
                            wp::TypeRef::Table(_)
                            | wp::TypeRef::Memory(_)
                            | wp::TypeRef::Global(_) => SymbolKind::Data,
                            wp::TypeRef::Tag(_) => SymbolKind::Unknown,
                        };

                        file.symbols.push(WasmSymbolInternal {
                            name: import.name,
                            address: 0,
                            size: 0,
                            kind,
                            section: SymbolSection::Undefined,
                            scope: SymbolScope::Dynamic,
                        });
                    }
                }
                wp::Payload::FunctionSection(section) => {
                    file.add_section(SectionId::Function, section.range(), "");
                    local_func_kinds =
                        vec![LocalFunctionKind::Unknown; section.into_iter().count()];
                }
                wp::Payload::TableSection(section) => {
                    file.add_section(SectionId::Table, section.range(), "");
                }
                wp::Payload::MemorySection(section) => {
                    file.add_section(SectionId::Memory, section.range(), "");
                }
                wp::Payload::GlobalSection(section) => {
                    file.add_section(SectionId::Global, section.range(), "");
                }
                wp::Payload::ExportSection(section) => {
                    file.add_section(SectionId::Export, section.range(), "");
                    if let Some(main_file_symbol) = main_file_symbol.take() {
                        file.symbols.push(main_file_symbol);
                    }

                    for export in section {
                        let export = export.read_error("Couldn't read an export item")?;

                        let (kind, section_idx) = match export.kind {
                            wp::ExternalKind::Func => {
                                if let Some(local_func_id) =
                                    export.index.checked_sub(imported_funcs_count)
                                {
                                    let local_func_kind =
                                        &mut local_func_kinds[local_func_id as usize];
                                    if let LocalFunctionKind::Unknown = local_func_kind {
                                        *local_func_kind = LocalFunctionKind::Exported {
                                            symbol_ids: Vec::new(),
                                        };
                                    }
                                    let symbol_ids = match local_func_kind {
                                        LocalFunctionKind::Exported { symbol_ids } => symbol_ids,
                                        _ => unreachable!(),
                                    };
                                    symbol_ids.push(file.symbols.len() as u32);
                                }
                                (SymbolKind::Text, SectionId::Code)
                            }
                            wp::ExternalKind::Table
                            | wp::ExternalKind::Memory
                            | wp::ExternalKind::Global => (SymbolKind::Data, SectionId::Data),
                            // TODO
                            wp::ExternalKind::Tag => continue,
                        };

                        file.symbols.push(WasmSymbolInternal {
                            name: export.name,
                            address: 0,
                            size: 0,
                            kind,
                            section: SymbolSection::Section(SectionIndex(section_idx as usize)),
                            scope: SymbolScope::Dynamic,
                        });
                    }
                }
                wp::Payload::StartSection { func, range, .. } => {
                    file.add_section(SectionId::Start, range, "");
                    entry_func_id = Some(func);
                }
                wp::Payload::ElementSection(section) => {
                    file.add_section(SectionId::Element, section.range(), "");
                }
                wp::Payload::CodeSectionStart { range, .. } => {
                    code_range_start = range.start;
                    file.add_section(SectionId::Code, range, "");
                    if let Some(main_file_symbol) = main_file_symbol.take() {
                        file.symbols.push(main_file_symbol);
                    }
                }
                wp::Payload::CodeSectionEntry(body) => {
                    let i = code_func_index;
                    code_func_index += 1;

                    let range = body.range();

                    let address = range.start as u64 - code_range_start as u64;
                    let size = (range.end - range.start) as u64;

                    if entry_func_id == Some(i as u32) {
                        file.entry = address;
                    }

                    let local_func_kind = &mut local_func_kinds[i];
                    match local_func_kind {
                        LocalFunctionKind::Unknown => {
                            *local_func_kind = LocalFunctionKind::Local {
                                symbol_id: file.symbols.len() as u32,
                            };
                            file.symbols.push(WasmSymbolInternal {
                                name: "",
                                address,
                                size,
                                kind: SymbolKind::Text,
                                section: SymbolSection::Section(SectionIndex(
                                    SectionId::Code as usize,
                                )),
                                scope: SymbolScope::Compilation,
                            });
                        }
                        LocalFunctionKind::Exported { symbol_ids } => {
                            for symbol_id in core::mem::take(symbol_ids) {
                                let export_symbol = &mut file.symbols[symbol_id as usize];
                                export_symbol.address = address;
                                export_symbol.size = size;
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                wp::Payload::DataSection(section) => {
                    file.add_section(SectionId::Data, section.range(), "");
                }
                wp::Payload::DataCountSection { range, .. } => {
                    file.add_section(SectionId::DataCount, range, "");
                }
                wp::Payload::CustomSection(section) => {
                    let name = section.name();
                    let size = section.data().len();
                    let mut range = section.range();
                    range.start = range.end - size;
                    file.add_section(SectionId::Custom, range, name);
                    if name == "name" {
                        for name in
                            wp::NameSectionReader::new(section.data(), section.data_offset())
                        {
                            // TODO: Right now, ill-formed name subsections
                            // are silently ignored in order to maintain
                            // compatibility with extended name sections, which
                            // are not yet supported by the version of
                            // `wasmparser` currently used.
                            // A better fix would be to update `wasmparser` to
                            // the newest version, but this requires
                            // a major rewrite of this file.
                            if let Ok(wp::Name::Function(name_map)) = name {
                                for naming in name_map {
                                    let naming =
                                        naming.read_error("Couldn't read a function name")?;
                                    if let Some(local_index) =
                                        naming.index.checked_sub(imported_funcs_count)
                                    {
                                        if let LocalFunctionKind::Local { symbol_id } =
                                            local_func_kinds[local_index as usize]
                                        {
                                            file.symbols[symbol_id as usize].name = naming.name;
                                        }
                                    }
                                }
                            }
                        }
                    } else if name.starts_with(".debug_") {
                        file.has_debug_symbols = true;
                    }
                }
                _ => {}
            }
        }

        Ok(file)
    }

    fn add_section(&mut self, id: SectionId, range: Range<usize>, name: &'data str) {
        let section = SectionHeader { id, range, name };
        self.id_sections[id as usize] = Some(self.sections.len());
        self.sections.push(section);
    }
}

impl<'data, R> read::private::Sealed for WasmFile<'data, R> {}

impl<'data, 'file, R: ReadRef<'data>> Object<'data, 'file> for WasmFile<'data, R>
where
    'data: 'file,
    R: 'file,
{
    type Segment = WasmSegment<'data, 'file, R>;
    type SegmentIterator = WasmSegmentIterator<'data, 'file, R>;
    type Section = WasmSection<'data, 'file, R>;
    type SectionIterator = WasmSectionIterator<'data, 'file, R>;
    type Comdat = WasmComdat<'data, 'file, R>;
    type ComdatIterator = WasmComdatIterator<'data, 'file, R>;
    type Symbol = WasmSymbol<'data, 'file>;
    type SymbolIterator = WasmSymbolIterator<'data, 'file>;
    type SymbolTable = WasmSymbolTable<'data, 'file>;
    type DynamicRelocationIterator = NoDynamicRelocationIterator;

    #[inline]
    fn architecture(&self) -> Architecture {
        Architecture::Wasm32
    }

    #[inline]
    fn is_little_endian(&self) -> bool {
        true
    }

    #[inline]
    fn is_64(&self) -> bool {
        false
    }

    fn kind(&self) -> ObjectKind {
        // TODO: check for `linking` custom section
        ObjectKind::Unknown
    }

    fn segments(&'file self) -> Self::SegmentIterator {
        WasmSegmentIterator { file: self }
    }

    fn section_by_name_bytes(
        &'file self,
        section_name: &[u8],
    ) -> Option<WasmSection<'data, 'file, R>> {
        self.sections()
            .find(|section| section.name_bytes() == Ok(section_name))
    }

    fn section_by_index(&'file self, index: SectionIndex) -> Result<WasmSection<'data, 'file, R>> {
        // TODO: Missing sections should return an empty section.
        let id_section = self
            .id_sections
            .get(index.0)
            .and_then(|x| *x)
            .read_error("Invalid Wasm section index")?;
        let section = self.sections.get(id_section).unwrap();
        Ok(WasmSection {
            file: self,
            section,
        })
    }

    fn sections(&'file self) -> Self::SectionIterator {
        WasmSectionIterator {
            file: self,
            sections: self.sections.iter(),
        }
    }

    fn comdats(&'file self) -> Self::ComdatIterator {
        WasmComdatIterator { file: self }
    }

    #[inline]
    fn symbol_by_index(&'file self, index: SymbolIndex) -> Result<WasmSymbol<'data, 'file>> {
        let symbol = self
            .symbols
            .get(index.0)
            .read_error("Invalid Wasm symbol index")?;
        Ok(WasmSymbol { index, symbol })
    }

    fn symbols(&'file self) -> Self::SymbolIterator {
        WasmSymbolIterator {
            symbols: self.symbols.iter().enumerate(),
        }
    }

    fn symbol_table(&'file self) -> Option<WasmSymbolTable<'data, 'file>> {
        Some(WasmSymbolTable {
            symbols: &self.symbols,
        })
    }

    fn dynamic_symbols(&'file self) -> Self::SymbolIterator {
        WasmSymbolIterator {
            symbols: [].iter().enumerate(),
        }
    }

    #[inline]
    fn dynamic_symbol_table(&'file self) -> Option<WasmSymbolTable<'data, 'file>> {
        None
    }

    #[inline]
    fn dynamic_relocations(&self) -> Option<NoDynamicRelocationIterator> {
        None
    }

    fn imports(&self) -> Result<Vec<Import<'data>>> {
        // TODO: return entries in the import section
        Ok(Vec::new())
    }

    fn exports(&self) -> Result<Vec<Export<'data>>> {
        // TODO: return entries in the export section
        Ok(Vec::new())
    }

    fn has_debug_symbols(&self) -> bool {
        self.has_debug_symbols
    }

    fn relative_address_base(&self) -> u64 {
        0
    }

    #[inline]
    fn entry(&'file self) -> u64 {
        self.entry
    }

    #[inline]
    fn flags(&self) -> FileFlags {
        FileFlags::None
    }
}

/// An iterator over the segments of a `WasmFile`.
#[derive(Debug)]
pub struct WasmSegmentIterator<'data, 'file, R = &'data [u8]> {
    #[allow(unused)]
    file: &'file WasmFile<'data, R>,
}

impl<'data, 'file, R> Iterator for WasmSegmentIterator<'data, 'file, R> {
    type Item = WasmSegment<'data, 'file, R>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

/// A segment of a `WasmFile`.
#[derive(Debug)]
pub struct WasmSegment<'data, 'file, R = &'data [u8]> {
    #[allow(unused)]
    file: &'file WasmFile<'data, R>,
}

impl<'data, 'file, R> read::private::Sealed for WasmSegment<'data, 'file, R> {}

impl<'data, 'file, R> ObjectSegment<'data> for WasmSegment<'data, 'file, R> {
    #[inline]
    fn address(&self) -> u64 {
        unreachable!()
    }

    #[inline]
    fn size(&self) -> u64 {
        unreachable!()
    }

    #[inline]
    fn align(&self) -> u64 {
        unreachable!()
    }

    #[inline]
    fn file_range(&self) -> (u64, u64) {
        unreachable!()
    }

    fn data(&self) -> Result<&'data [u8]> {
        unreachable!()
    }

    fn data_range(&self, _address: u64, _size: u64) -> Result<Option<&'data [u8]>> {
        unreachable!()
    }

    #[inline]
    fn name_bytes(&self) -> Result<Option<&[u8]>> {
        unreachable!()
    }

    #[inline]
    fn name(&self) -> Result<Option<&str>> {
        unreachable!()
    }

    #[inline]
    fn flags(&self) -> SegmentFlags {
        unreachable!()
    }
}

/// An iterator over the sections of a `WasmFile`.
#[derive(Debug)]
pub struct WasmSectionIterator<'data, 'file, R = &'data [u8]> {
    file: &'file WasmFile<'data, R>,
    sections: slice::Iter<'file, SectionHeader<'data>>,
}

impl<'data, 'file, R> Iterator for WasmSectionIterator<'data, 'file, R> {
    type Item = WasmSection<'data, 'file, R>;

    fn next(&mut self) -> Option<Self::Item> {
        let section = self.sections.next()?;
        Some(WasmSection {
            file: self.file,
            section,
        })
    }
}

/// A section of a `WasmFile`.
#[derive(Debug)]
pub struct WasmSection<'data, 'file, R = &'data [u8]> {
    file: &'file WasmFile<'data, R>,
    section: &'file SectionHeader<'data>,
}

impl<'data, 'file, R> read::private::Sealed for WasmSection<'data, 'file, R> {}

impl<'data, 'file, R: ReadRef<'data>> ObjectSection<'data> for WasmSection<'data, 'file, R> {
    type RelocationIterator = WasmRelocationIterator<'data, 'file, R>;

    #[inline]
    fn index(&self) -> SectionIndex {
        // Note that we treat all custom sections as index 0.
        // This is ok because they are never looked up by index.
        SectionIndex(self.section.id as usize)
    }

    #[inline]
    fn address(&self) -> u64 {
        0
    }

    #[inline]
    fn size(&self) -> u64 {
        let range = &self.section.range;
        (range.end - range.start) as u64
    }

    #[inline]
    fn align(&self) -> u64 {
        1
    }

    #[inline]
    fn file_range(&self) -> Option<(u64, u64)> {
        let range = &self.section.range;
        Some((range.start as _, range.end as _))
    }

    #[inline]
    fn data(&self) -> Result<&'data [u8]> {
        let range = &self.section.range;
        self.file
            .data
            .read_bytes_at(range.start as u64, range.end as u64 - range.start as u64)
            .read_error("Invalid Wasm section size or offset")
    }

    fn data_range(&self, _address: u64, _size: u64) -> Result<Option<&'data [u8]>> {
        unimplemented!()
    }

    #[inline]
    fn compressed_file_range(&self) -> Result<CompressedFileRange> {
        Ok(CompressedFileRange::none(self.file_range()))
    }

    #[inline]
    fn compressed_data(&self) -> Result<CompressedData<'data>> {
        self.data().map(CompressedData::none)
    }

    #[inline]
    fn name_bytes(&self) -> Result<&[u8]> {
        self.name().map(str::as_bytes)
    }

    #[inline]
    fn name(&self) -> Result<&str> {
        Ok(match self.section.id {
            SectionId::Custom => self.section.name,
            SectionId::Type => "<type>",
            SectionId::Import => "<import>",
            SectionId::Function => "<function>",
            SectionId::Table => "<table>",
            SectionId::Memory => "<memory>",
            SectionId::Global => "<global>",
            SectionId::Export => "<export>",
            SectionId::Start => "<start>",
            SectionId::Element => "<element>",
            SectionId::Code => "<code>",
            SectionId::Data => "<data>",
            SectionId::DataCount => "<data_count>",
        })
    }

    #[inline]
    fn segment_name_bytes(&self) -> Result<Option<&[u8]>> {
        Ok(None)
    }

    #[inline]
    fn segment_name(&self) -> Result<Option<&str>> {
        Ok(None)
    }

    #[inline]
    fn kind(&self) -> SectionKind {
        match self.section.id {
            SectionId::Custom => match self.section.name {
                "reloc." | "linking" => SectionKind::Linker,
                _ => SectionKind::Other,
            },
            SectionId::Type => SectionKind::Metadata,
            SectionId::Import => SectionKind::Linker,
            SectionId::Function => SectionKind::Metadata,
            SectionId::Table => SectionKind::UninitializedData,
            SectionId::Memory => SectionKind::UninitializedData,
            SectionId::Global => SectionKind::Data,
            SectionId::Export => SectionKind::Linker,
            SectionId::Start => SectionKind::Linker,
            SectionId::Element => SectionKind::Data,
            SectionId::Code => SectionKind::Text,
            SectionId::Data => SectionKind::Data,
            SectionId::DataCount => SectionKind::UninitializedData,
        }
    }

    #[inline]
    fn relocations(&self) -> WasmRelocationIterator<'data, 'file, R> {
        WasmRelocationIterator(PhantomData)
    }

    #[inline]
    fn flags(&self) -> SectionFlags {
        SectionFlags::None
    }
}

/// An iterator over the COMDAT section groups of a `WasmFile`.
#[derive(Debug)]
pub struct WasmComdatIterator<'data, 'file, R = &'data [u8]> {
    #[allow(unused)]
    file: &'file WasmFile<'data, R>,
}

impl<'data, 'file, R> Iterator for WasmComdatIterator<'data, 'file, R> {
    type Item = WasmComdat<'data, 'file, R>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

/// A COMDAT section group of a `WasmFile`.
#[derive(Debug)]
pub struct WasmComdat<'data, 'file, R = &'data [u8]> {
    #[allow(unused)]
    file: &'file WasmFile<'data, R>,
}

impl<'data, 'file, R> read::private::Sealed for WasmComdat<'data, 'file, R> {}

impl<'data, 'file, R> ObjectComdat<'data> for WasmComdat<'data, 'file, R> {
    type SectionIterator = WasmComdatSectionIterator<'data, 'file, R>;

    #[inline]
    fn kind(&self) -> ComdatKind {
        unreachable!();
    }

    #[inline]
    fn symbol(&self) -> SymbolIndex {
        unreachable!();
    }

    #[inline]
    fn name_bytes(&self) -> Result<&[u8]> {
        unreachable!();
    }

    #[inline]
    fn name(&self) -> Result<&str> {
        unreachable!();
    }

    #[inline]
    fn sections(&self) -> Self::SectionIterator {
        unreachable!();
    }
}

/// An iterator over the sections in a COMDAT section group of a `WasmFile`.
#[derive(Debug)]
pub struct WasmComdatSectionIterator<'data, 'file, R = &'data [u8]> {
    #[allow(unused)]
    file: &'file WasmFile<'data, R>,
}

impl<'data, 'file, R> Iterator for WasmComdatSectionIterator<'data, 'file, R> {
    type Item = SectionIndex;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

/// A symbol table of a `WasmFile`.
#[derive(Debug)]
pub struct WasmSymbolTable<'data, 'file> {
    symbols: &'file [WasmSymbolInternal<'data>],
}

impl<'data, 'file> read::private::Sealed for WasmSymbolTable<'data, 'file> {}

impl<'data, 'file> ObjectSymbolTable<'data> for WasmSymbolTable<'data, 'file> {
    type Symbol = WasmSymbol<'data, 'file>;
    type SymbolIterator = WasmSymbolIterator<'data, 'file>;

    fn symbols(&self) -> Self::SymbolIterator {
        WasmSymbolIterator {
            symbols: self.symbols.iter().enumerate(),
        }
    }

    fn symbol_by_index(&self, index: SymbolIndex) -> Result<Self::Symbol> {
        let symbol = self
            .symbols
            .get(index.0)
            .read_error("Invalid Wasm symbol index")?;
        Ok(WasmSymbol { index, symbol })
    }
}

/// An iterator over the symbols of a `WasmFile`.
#[derive(Debug)]
pub struct WasmSymbolIterator<'data, 'file> {
    symbols: core::iter::Enumerate<slice::Iter<'file, WasmSymbolInternal<'data>>>,
}

impl<'data, 'file> Iterator for WasmSymbolIterator<'data, 'file> {
    type Item = WasmSymbol<'data, 'file>;

    fn next(&mut self) -> Option<Self::Item> {
        let (index, symbol) = self.symbols.next()?;
        Some(WasmSymbol {
            index: SymbolIndex(index),
            symbol,
        })
    }
}

/// A symbol of a `WasmFile`.
#[derive(Clone, Copy, Debug)]
pub struct WasmSymbol<'data, 'file> {
    index: SymbolIndex,
    symbol: &'file WasmSymbolInternal<'data>,
}

#[derive(Clone, Debug)]
struct WasmSymbolInternal<'data> {
    name: &'data str,
    address: u64,
    size: u64,
    kind: SymbolKind,
    section: SymbolSection,
    scope: SymbolScope,
}

impl<'data, 'file> read::private::Sealed for WasmSymbol<'data, 'file> {}

impl<'data, 'file> ObjectSymbol<'data> for WasmSymbol<'data, 'file> {
    #[inline]
    fn index(&self) -> SymbolIndex {
        self.index
    }

    #[inline]
    fn name_bytes(&self) -> read::Result<&'data [u8]> {
        Ok(self.symbol.name.as_bytes())
    }

    #[inline]
    fn name(&self) -> read::Result<&'data str> {
        Ok(self.symbol.name)
    }

    #[inline]
    fn address(&self) -> u64 {
        self.symbol.address
    }

    #[inline]
    fn size(&self) -> u64 {
        self.symbol.size
    }

    #[inline]
    fn kind(&self) -> SymbolKind {
        self.symbol.kind
    }

    #[inline]
    fn section(&self) -> SymbolSection {
        self.symbol.section
    }

    #[inline]
    fn is_undefined(&self) -> bool {
        self.symbol.section == SymbolSection::Undefined
    }

    #[inline]
    fn is_definition(&self) -> bool {
        self.symbol.kind == SymbolKind::Text && self.symbol.section != SymbolSection::Undefined
    }

    #[inline]
    fn is_common(&self) -> bool {
        self.symbol.section == SymbolSection::Common
    }

    #[inline]
    fn is_weak(&self) -> bool {
        false
    }

    #[inline]
    fn scope(&self) -> SymbolScope {
        self.symbol.scope
    }

    #[inline]
    fn is_global(&self) -> bool {
        self.symbol.scope != SymbolScope::Compilation
    }

    #[inline]
    fn is_local(&self) -> bool {
        self.symbol.scope == SymbolScope::Compilation
    }

    #[inline]
    fn flags(&self) -> SymbolFlags<SectionIndex, SymbolIndex> {
        SymbolFlags::None
    }
}

/// An iterator over the relocations in a `WasmSection`.
#[derive(Debug)]
pub struct WasmRelocationIterator<'data, 'file, R = &'data [u8]>(
    PhantomData<(&'data (), &'file (), R)>,
);

impl<'data, 'file, R> Iterator for WasmRelocationIterator<'data, 'file, R> {
    type Item = (u64, Relocation);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}
