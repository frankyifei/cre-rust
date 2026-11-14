//! PalmDB container, MOBI header and INDX index parsing shared by the MOBI 6
//! (`cre-mobi`) and KF8/AZW3 (`cre-kf8`) readers.
//!
//! Both formats are Palm Database files: a fixed header, a table of record
//! offsets, then the records. Record 0 holds a PalmDOC header followed by a
//! MOBI header (and usually an EXTH block); records 1..=text_record_count hold
//! the text; the rest holds images, fonts and index records.
//!
//! References: crengine `src/pdbfmt.cpp` (container, PalmDOC decompression,
//! trailing entries, INDX/TAGX/CNCX) and calibre
//! `calibre/ebooks/mobi/reader/{headers,index}.py`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

pub mod html;

/// `0xFFFFFFFF` in a header field means "not present".
pub const NULL_INDEX: u32 = 0xFFFF_FFFF;

const PDB_HEADER_LEN: usize = 78;

/// The file bytes a [`Pdb`] reads its records from.
///
/// A MOBI/AZW3 holds every image and font in the PalmDB, but a reader only
/// decodes the pictures a page actually shows. Reading the file into a `Vec`
/// put all of it on the anonymous heap for the length of the parse; a mapped
/// file only faults in the records that are touched, and the pages it does
/// touch stay in the page cache, which the kernel can reclaim.
enum PdbData {
    Owned(Arc<[u8]>),
    Mapped(Arc<memmap2::Mmap>),
}

impl std::ops::Deref for PdbData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Owned(data) => data,
            Self::Mapped(mapped) => mapped,
        }
    }
}

/// The bytes a [`BookResources`] slices its records from.
///
/// The reader keeps this alive for the session instead of copying every image
/// out of the file: on a mapped file the pictures stay page cache, so a
/// 25 MB comic costs no anonymous memory until a page actually paints one.
pub enum ResourceBacking {
    Owned(Arc<[u8]>),
    Mapped(Arc<memmap2::Mmap>),
}

impl std::ops::Deref for ResourceBacking {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Owned(data) => data,
            Self::Mapped(mapped) => mapped,
        }
    }
}

impl ResourceBacking {
    /// Drops the mapped pages this process has faulted in.
    ///
    /// The parse touches the text records, the index records and the headers
    /// of every image record, and the open-time layout pass touches the
    /// pictures again to learn their intrinsic size. None of those pages are
    /// needed after the book is assembled: the index holds offsets, and the
    /// first page that paints a picture faults it back in from the page cache.
    /// Measured on a 43 MB AZW3: 25 MB of resident file pages drop to none.
    pub fn drop_pages(&self) {
        if let Self::Mapped(mapped) = self {
            // Resource reads are individual records, so readahead and
            // fault-around would drag neighbouring pictures into RSS for
            // nothing.
            let _ = mapped.advise(memmap2::Advice::Random);
            // SAFETY: the caller only runs this after the book is assembled,
            // when no slice borrowed from the mapping is alive; the ranges in
            // the index are offsets, and the next `get` faults the pages back
            // in from the page cache.
            let _ = unsafe { mapped.unchecked_advise(memmap2::UncheckedAdvice::DontNeed) };
        }
    }
}

/// A named byte range of the book file, for the resource index.
pub type ResourceRange = (String, std::ops::Range<usize>);

/// Resource name to record range (or owned bytes), backed by the book file.
///
/// MOBI and KF8 images are PalmDB records that are still in the file, so the
/// index only records where each one starts and ends. `get` slices the mapped
/// file on demand; nothing is copied at open time except the cover.
#[derive(Default)]
pub struct BookResources {
    backing: Option<ResourceBacking>,
    ranges: HashMap<String, std::ops::Range<usize>>,
    owned: HashMap<String, Arc<[u8]>>,
}

impl BookResources {
    pub fn new(backing: ResourceBacking) -> Self {
        Self {
            backing: Some(backing),
            ..Self::default()
        }
    }

    /// Records that a resource lives at `range` of the backing bytes.
    pub fn insert_range(&mut self, name: impl Into<String>, range: std::ops::Range<usize>) {
        self.ranges.insert(name.into(), range);
    }

    /// Records a resource the reader generated (no range in the file).
    pub fn insert_owned(&mut self, name: impl Into<String>, bytes: Arc<[u8]>) {
        self.owned.insert(name.into(), bytes);
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        if let Some(bytes) = self.owned.get(name) {
            return Some(bytes);
        }
        let range = self.ranges.get(name)?;
        self.backing.as_deref()?.get(range.clone())
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.owned.contains_key(name) || self.ranges.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.owned.len() + self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of the resources the index can reach.
    ///
    /// For a mapped file these are page-cache bytes, not anonymous memory.
    pub fn bytes(&self) -> usize {
        self.owned.values().map(|bytes| bytes.len()).sum::<usize>()
            + self
                .ranges
                .values()
                .map(|range| range.end.saturating_sub(range.start))
                .sum::<usize>()
    }

    /// Drops the file pages the parse faulted in; pictures are re-read from
    /// the page cache when a page paints them.
    pub fn drop_pages(&self) {
        if let Some(backing) = self.backing.as_ref() {
            backing.drop_pages();
        }
    }

    /// Whether the pictures come from the file mapping rather than copies.
    pub fn is_mapped(&self) -> bool {
        matches!(self.backing, Some(ResourceBacking::Mapped(_)))
    }
}

/// A PalmDB record index is exactly what a mapped container looks like to the
/// reader: names to byte ranges, sliced out of the file on demand.
impl cre_resource::ResourceIndex for BookResources {
    fn get(&self, key: &str) -> Option<&[u8]> {
        BookResources::get(self, key)
    }

    fn contains_key(&self, key: &str) -> bool {
        BookResources::contains_key(self, key)
    }

    fn len(&self) -> usize {
        BookResources::len(self)
    }

    fn bytes(&self) -> usize {
        BookResources::bytes(self)
    }

    fn drop_pages(&self) {
        BookResources::drop_pages(self);
    }
}

/// A Palm Database file: header fields plus the record table.
pub struct Pdb {
    data: PdbData,
    offsets: Vec<usize>,
    pub name: String,
    /// 4-byte type and creator, e.g. `BOOK`/`MOBI`.
    pub book_type: String,
    pub creator: String,
}

impl Pdb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        // Only the records the reader asks for are faulted in, so the 36 MB of
        // compressed resources in a typical AZW3 never join the open's RSS.
        // Mapping also avoids a whole-file `read` on an anonymous heap.
        if let Ok(file) = std::fs::File::open(path)
            && let Ok(mapped) = unsafe { memmap2::Mmap::map(&file) }
        {
            return Self::from_data(PdbData::Mapped(Arc::new(mapped)));
        }
        let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Self::from_data(PdbData::Owned(Arc::from(data)))
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        Self::from_data(PdbData::Owned(Arc::from(data)))
    }

    /// Hands the file bytes to a [`BookResources`] index, so records can be
    /// sliced for the rest of the session without being copied.
    pub fn into_backing(self) -> ResourceBacking {
        match self.data {
            PdbData::Owned(data) => ResourceBacking::Owned(data),
            PdbData::Mapped(mapped) => ResourceBacking::Mapped(mapped),
        }
    }

    /// Byte range of one record in the file.
    pub fn record_range(&self, index: usize) -> Option<std::ops::Range<usize>> {
        let start = *self.offsets.get(index)?;
        let end = self
            .offsets
            .get(index + 1)
            .copied()
            .unwrap_or(self.data.len());
        (start <= end && end <= self.data.len()).then_some(start..end)
    }

    fn from_data(data: PdbData) -> Result<Self> {
        if data.len() < PDB_HEADER_LEN {
            bail!("not a PalmDB file: only {} bytes", data.len());
        }
        let name = latin1_trimmed(&data[..32]);
        let book_type = latin1(&data[60..64]);
        let creator = latin1(&data[64..68]);
        let count = u16::from_be_bytes([data[76], data[77]]) as usize;
        if count == 0 {
            bail!("PalmDB file has no records");
        }
        if PDB_HEADER_LEN + count * 8 > data.len() {
            bail!("PalmDB record table is truncated");
        }
        // The stored offsets are absolute file offsets, as crengine and
        // calibre both treat them.
        let mut offsets = Vec::with_capacity(count);
        for index in 0..count {
            let at = PDB_HEADER_LEN + index * 8;
            let offset =
                u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
            if offset > data.len() {
                bail!("PalmDB record {index} starts past the end of the file");
            }
            offsets.push(offset);
        }
        if offsets[0] < PDB_HEADER_LEN {
            bail!("PalmDB record data overlaps the header");
        }
        Ok(Self {
            data,
            offsets,
            name,
            book_type,
            creator,
        })
    }

    pub fn records(&self) -> usize {
        self.offsets.len()
    }

    pub fn record(&self, index: usize) -> Option<&[u8]> {
        let start = *self.offsets.get(index)?;
        let end = self
            .offsets
            .get(index + 1)
            .copied()
            .unwrap_or(self.data.len());
        (start <= end && end <= self.data.len()).then(|| &self.data[start..end])
    }

    /// True when this is a book we understand (MOBI or plain PalmDOC).
    pub fn is_mobi(&self) -> bool {
        (self.book_type == "BOOK" && self.creator == "MOBI")
            || (self.book_type == "TEXt" && self.creator == "REAd")
    }

    /// True when the file at `path` starts with a PalmDB/MOBI header, without
    /// reading the whole file.
    pub fn looks_like_mobi(data: &[u8]) -> bool {
        data.len() >= 68 && (&data[60..68] == b"BOOKMOBI" || &data[60..68] == b"TEXTREAd")
    }
}

/// Text encoding used for the HTML and for index strings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    Utf8,
    Cp1252,
}

impl Encoding {
    pub fn from_header(value: u32) -> Self {
        match value {
            65001 => Encoding::Utf8,
            _ => Encoding::Cp1252,
        }
    }

    pub fn decode(self, bytes: &[u8]) -> String {
        match self {
            Encoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
            Encoding::Cp1252 => cp1252(bytes),
        }
    }

    /// The IANA name, for metadata.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Cp1252 => "windows-1252",
        }
    }
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| *byte as char).collect()
}

fn latin1_trimmed(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    latin1(&bytes[..end]).trim().to_string()
}

/// CP1252 decoding: 0x80..=0x9F differ from Latin-1, everything else matches.
pub fn cp1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{81}', '\u{201A}', '\u{192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{2C6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8D}', '\u{17D}',
        '\u{8F}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}',
        '\u{2014}', '\u{2DC}', '\u{2122}', '\u{161}', '\u{203A}', '\u{153}', '\u{9D}', '\u{17E}',
        '\u{178}',
    ];
    let mut out = String::with_capacity(bytes.len());
    for byte in bytes {
        match byte {
            0x80..=0x9F => out.push(HIGH[(byte - 0x80) as usize]),
            _ => out.push(*byte as char),
        }
    }
    out
}

/// The MOBI header in record 0 (following the 16-byte PalmDOC header).
#[derive(Clone, Debug)]
pub struct MobiHeader {
    pub compression: u16,
    pub text_length: u32,
    pub text_record_count: usize,
    pub encryption: u16,
    pub magic: [u8; 4],
    pub header_length: u32,
    pub mobi_type: u32,
    pub encoding: Encoding,
    pub version: u32,
    pub full_name: String,
    pub first_image_index: u32,
    pub extra_data_flags: u16,
    pub exth: Exth,
    /// Table of contents index (INDX), at offset 0xF4. For KF8 files this is
    /// the KF8 NCX index.
    pub ncx_index: u32,
    // KF8 (AZW3) fields, all `NULL_INDEX` for MOBI 6 files.
    pub fdst_index: u32,
    pub fdst_count: u32,
    pub skel_index: u32,
    pub frag_index: u32,
    pub guide_index: u32,
    pub kf8_data_index: u32,
}

impl MobiHeader {
    pub fn parse(record0: &[u8]) -> Result<Self> {
        if record0.len() < 16 + 24 {
            bail!("MOBI record 0 is too short");
        }
        let compression = be16(record0, 0)?;
        let text_length = be32(record0, 4)?;
        let text_record_count = be16(record0, 8)? as usize;
        let encryption = be16(record0, 12)?;
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&record0[16..20]);
        let header_length = be32(record0, 20)?;
        let mobi_type = be32(record0, 24)?;
        let encoding = Encoding::from_header(be32(record0, 28)?);
        let version = be32(record0, 36)?;
        let full_name_offset = be32(record0, 84)? as usize;
        let full_name_length = be32(record0, 88)? as usize;
        let first_image_index = be32(record0, 108)?;
        let extra_data_flags = if record0.len() >= 244 {
            be16(record0, 242)?
        } else {
            0
        };

        let full_name = if full_name_offset > 0
            && full_name_length > 0
            && full_name_offset + full_name_length <= record0.len()
        {
            encoding.decode(&record0[full_name_offset..full_name_offset + full_name_length])
        } else {
            String::new()
        };

        // EXTH flags live at 0x80 of record 0; bit 6 means an EXTH block
        // follows the MOBI header.
        let flags = if record0.len() >= 0x84 {
            be32(record0, 0x80)?
        } else {
            0
        };
        let exth = if flags & 0x40 != 0 && 16 + header_length as usize + 12 <= record0.len() {
            Exth::parse(&record0[16 + header_length as usize..]).unwrap_or_else(Exth::empty)
        } else {
            Exth::empty()
        };

        let ncx_index = if record0.len() >= 0xF8 {
            be32(record0, 0xF4)?
        } else {
            NULL_INDEX
        };

        // KF8-only header fields (calibre's headers.py):
        //   0xC0 FDST record, 0xC4 flow count,
        //   0xF8 fragment (div) table, 0xFC skeleton table,
        //   0x100 datp table, 0x104 guide ("other") table.
        let (fdst_index, fdst_count, skel_index, frag_index, guide_index, kf8_data_index) =
            if version == 8 && record0.len() >= 0xF8 + 16 {
                let frag = be32(record0, 0xF8)?;
                let skel = be32(record0, 0xFC)?;
                let datp = be32(record0, 0x100)?;
                let guide = be32(record0, 0x104)?;
                let fdst = be32(record0, 0xC0)?;
                let fdst_count = be32(record0, 0xC4)?;
                (
                    // A single flow means the FDST record is bogus.
                    if fdst_count > 1 { fdst } else { NULL_INDEX },
                    fdst_count,
                    skel,
                    frag,
                    guide,
                    datp,
                )
            } else {
                (
                    NULL_INDEX, 0, NULL_INDEX, NULL_INDEX, NULL_INDEX, NULL_INDEX,
                )
            };

        Ok(Self {
            compression,
            text_length,
            text_record_count,
            encryption,
            magic,
            header_length,
            mobi_type,
            encoding,
            version,
            full_name,
            first_image_index,
            extra_data_flags,
            exth,
            ncx_index,
            fdst_index,
            fdst_count,
            skel_index,
            frag_index,
            guide_index,
            kf8_data_index,
        })
    }

    /// True for a KF8 (AZW3) payload: version 8 with a skeleton table.
    pub fn is_kf8(&self) -> bool {
        self.version == 8 && self.skel_index != NULL_INDEX
    }

    /// True when the text uses the HUFF/CDIC algorithm, which is not
    /// implemented (very old Kindlegen output).
    pub fn is_huff(&self) -> bool {
        self.compression == 17480
    }
}

/// EXTH metadata block: a list of (tag, value) pairs.
#[derive(Clone, Debug, Default)]
pub struct Exth {
    entries: Vec<(u32, Vec<u8>)>,
}

impl Exth {
    pub fn empty() -> Self {
        Self::default()
    }

    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 || &data[..4] != b"EXTH" {
            return None;
        }
        let length = be32(data, 4).ok()? as usize;
        let count = be32(data, 8).ok()? as usize;
        let end = length.max(12).min(data.len());
        let mut entries = Vec::with_capacity(count);
        let mut at = 12;
        for _ in 0..count {
            if at + 8 > end {
                break;
            }
            let tag = be32(data, at).ok()?;
            let size = be32(data, at + 4).ok()? as usize;
            if size < 8 || at + size > data.len() {
                break;
            }
            // Values stay as raw bytes: the text tags are decoded by the
            // caller with the book's encoding.
            entries.push((tag, data[at + 8..at + size].to_vec()));
            at += size;
        }
        Some(Self { entries })
    }

    pub fn get(&self, tag: u32) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(entry_tag, _)| *entry_tag == tag)
            .map(|(_, value)| value.as_slice())
    }

    pub fn all(&self, tag: u32) -> impl Iterator<Item = &[u8]> {
        self.entries
            .iter()
            .filter(move |(entry_tag, _)| *entry_tag == tag)
            .map(|(_, value)| value.as_slice())
    }

    pub fn string(&self, tag: u32, encoding: Encoding) -> Option<String> {
        let value = self.get(tag)?;
        let text = encoding.decode(value).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    pub fn joined_strings(&self, tag: u32, encoding: Encoding) -> Option<String> {
        let parts: Vec<String> = self
            .all(tag)
            .map(|value| encoding.decode(value).trim().to_string())
            .filter(|text| !text.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join(" / "))
    }

    pub fn uint(&self, tag: u32) -> Option<u32> {
        let value = self.get(tag)?;
        if value.len() != 4 {
            return None;
        }
        Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
    }
}

/// Reads a big-endian u16 at `at`.
pub fn be16(data: &[u8], at: usize) -> Result<u16> {
    let bytes = data
        .get(at..at + 2)
        .with_context(|| format!("u16 at {at} is out of bounds"))?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Reads a big-endian u32 at `at`.
pub fn be32(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data
        .get(at..at + 4)
        .with_context(|| format!("u32 at {at} is out of bounds"))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Extracts and decompresses the text records `first..first + count`.
///
/// `first` is 1 for MOBI 6 (the text starts in record 1) and also 1 for a
/// standalone KF8 file.
pub fn extract_text(pdb: &Pdb, header: &MobiHeader, first: usize, count: usize) -> Result<Vec<u8>> {
    if header.is_huff() {
        bail!("HUFF/CDIC compressed MOBI text is not supported");
    }
    let mut out = Vec::with_capacity(header.text_length as usize);
    for index in first..first + count {
        let Some(record) = pdb.record(index) else {
            break;
        };
        let trimmed = strip_trailing_entries(record, header.extra_data_flags);
        match header.compression {
            1 => out.extend_from_slice(trimmed),
            2 => palm_doc_decompress_into(trimmed, &mut out),
            other => bail!("unsupported MOBI compression {other}"),
        }
    }
    // Some generators leave a terminator byte behind.
    if out.last() == Some(&b'#') {
        out.pop();
    }
    Ok(out)
}

/// Removes the "trailing entries" some writers append to each text record
/// (multibyte character overlap and other per-record data), as described by the
/// extra-data flags in the MOBI header. Follows calibre's
/// `sizeof_trailing_entries`.
pub fn strip_trailing_entries(data: &[u8], flags: u16) -> &[u8] {
    if flags == 0 {
        return data;
    }
    let mut total = 0usize;
    let mut remaining_flags = flags >> 1;
    while remaining_flags != 0 {
        if remaining_flags & 1 != 0 {
            let Some(size) = trailing_entry_size(data, data.len().saturating_sub(total)) else {
                return data;
            };
            total += size;
        }
        remaining_flags >>= 1;
    }
    if flags & 1 != 0 {
        let at = data.len().saturating_sub(total + 1);
        match data.get(at) {
            Some(byte) => total += (byte & 0x3) as usize + 1,
            None => total += 1,
        }
    }
    data.get(..data.len().saturating_sub(total)).unwrap_or(data)
}

fn trailing_entry_size(data: &[u8], end: usize) -> Option<usize> {
    let mut bit = 0u32;
    let mut result = 0usize;
    let mut at = end;
    loop {
        if at == 0 || at > data.len() {
            return None;
        }
        at -= 1;
        let value = data[at];
        result |= ((value & 0x7F) as usize) << bit;
        bit += 7;
        if value & 0x80 != 0 || bit >= 28 || at == 0 {
            return Some(result);
        }
    }
}

/// PalmDOC (LZ77) decompression.
pub fn palm_doc_decompress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 4);
    palm_doc_decompress_into(data, &mut out);
    out
}

fn palm_doc_decompress_into(data: &[u8], out: &mut Vec<u8>) {
    let mut at = 0usize;
    while at < data.len() {
        let byte = data[at];
        at += 1;
        match byte {
            1..=8 => {
                let count = byte as usize;
                let end = (at + count).min(data.len());
                out.extend_from_slice(&data[at..end]);
                at = end;
            }
            0x09..=0x7F => out.push(byte),
            0xC0..=0xFF => {
                out.push(b' ');
                out.push(byte & 0x7F);
            }
            _ => {
                // 0x80..=0xBF: two-byte back reference.
                let Some(next) = data.get(at) else { break };
                at += 1;
                let pair = (((byte & 0x3F) as usize) << 8) | *next as usize;
                let distance = pair >> 3;
                let length = (pair & 0x7) + 3;
                if distance == 0 || distance > out.len() {
                    // A corrupt reference: emit a placeholder rather than loop.
                    out.push(b'?');
                    continue;
                }
                let start = out.len() - distance;
                for offset in 0..length {
                    let value = out[start + offset];
                    out.push(value);
                }
            }
        }
    }
}

/// One entry of an INDX record.
#[derive(Clone, Debug, Default)]
pub struct IndexEntry {
    /// The entry key: a string for a MOBI 6 NCX, a number for the KF8 tables.
    pub ident: Vec<u8>,
    /// Tag number -> values, as encoded by the TAGX table.
    pub tags: BTreeMap<u32, Vec<u32>>,
}

impl IndexEntry {
    pub fn ident_number(&self) -> Option<u32> {
        std::str::from_utf8(&self.ident).ok()?.trim().parse().ok()
    }

    pub fn tag(&self, tag: u32) -> Option<u32> {
        self.tags
            .get(&tag)
            .and_then(|values| values.first().copied())
    }

    pub fn tag_pair(&self, tag: u32) -> Option<(u32, u32)> {
        let values = self.tags.get(&tag)?;
        match values.as_slice() {
            [first, second, ..] => Some((*first, *second)),
            [only] => Some((*only, 0)),
            _ => None,
        }
    }
}

/// A parsed INDX table plus its CNCX string pool.
#[derive(Clone, Debug, Default)]
pub struct Index {
    pub entries: Vec<IndexEntry>,
    /// CNCX offset -> string, using crengine's `record * 0x10000` base.
    pub cncx: HashMap<u32, String>,
}

impl Index {
    pub fn string(&self, offset: u32) -> Option<&str> {
        self.cncx.get(&offset).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Parses the INDX records starting at PDB record `index`, following crengine's
/// `readMobiToc` (which also matches calibre's `index.py`).
pub fn read_index(pdb: &Pdb, index: usize, encoding: Encoding) -> Result<Index> {
    let header = pdb
        .record(index)
        .with_context(|| format!("INDX record {index} is missing"))?;
    if header.len() < 0xC0 || &header[..4] != b"INDX" {
        bail!("record {index} is not an INDX header");
    }
    let index_record_count = be32(header, 4 + 5 * 4)? as usize;
    let cncx_count = be32(header, 4 + 12 * 4)? as usize;
    if index_record_count == 0 || index_record_count > 0xFFFF || cncx_count > 0xFFFF {
        bail!("INDX record {index} has an implausible block count");
    }
    let mut tagx_at = be32(header, 4 + 44 * 4)? as usize;
    if tagx_at + 12 > header.len() || &header[tagx_at..tagx_at + 4] != b"TAGX" {
        // Some writers put junk there; crengine and calibre both retry at the
        // header length field before giving up.
        let alternative = be32(header, 4)? as usize;
        if alternative + 12 <= header.len() && &header[alternative..alternative + 4] == b"TAGX" {
            tagx_at = alternative;
        } else {
            bail!("INDX record {index} has no TAGX section");
        }
    }
    let first_entry_offset = be32(header, tagx_at + 4)? as usize;
    let control_byte_count = be32(header, tagx_at + 8)? as usize;
    if control_byte_count == 0 || control_byte_count > 32 {
        bail!("INDX record {index} has an invalid control byte count");
    }
    if first_entry_offset <= 12 || tagx_at + first_entry_offset > header.len() {
        bail!("INDX record {index} has an invalid TAGX size");
    }
    let mut tagx = Vec::new();
    let mut at = 12;
    while at + 4 <= first_entry_offset && tagx.len() < 64 {
        let base = tagx_at + at;
        tagx.push(Tagx {
            tag: header[base] as u32,
            values: header[base + 1] as u32,
            mask: header[base + 2],
            end: header[base + 3] == 1,
        });
        at += 4;
    }

    // CNCX string pool.
    let mut cncx = HashMap::new();
    for chunk in 0..cncx_count {
        let record_index = index + 1 + index_record_count + chunk;
        let Some(record) = pdb.record(record_index) else {
            break;
        };
        let base = (chunk as u32).wrapping_mul(0x1_0000);
        let mut at = 0usize;
        while at < record.len() {
            let start = at;
            let Some(length) = read_vwi(record, &mut at) else {
                break;
            };
            let length = length as usize;
            if at + length > record.len() {
                break;
            }
            let text = encoding.decode(&record[at..at + length]);
            cncx.insert(base.wrapping_add(start as u32), text);
            at += length;
        }
    }

    let mut entries = Vec::new();
    for record_number in 0..index_record_count {
        let record_index = index + 1 + record_number;
        let Some(record) = pdb.record(record_index) else {
            break;
        };
        if record.len() < 12 || &record[..4] != b"INDX" {
            break;
        }
        let idxt_at = be32(record, 4 + 4 * 4)? as usize;
        let entry_count = be32(record, 4 + 5 * 4)? as usize;
        if entry_count > 0xFFFF || idxt_at + 4 + 2 * entry_count > record.len() {
            continue;
        }
        if &record[idxt_at..idxt_at + 4] != b"IDXT" {
            continue;
        }
        for entry_number in 0..entry_count {
            let start = be16(record, idxt_at + 4 + 2 * entry_number)? as usize;
            let end = if entry_number + 1 < entry_count {
                be16(record, idxt_at + 4 + 2 * (entry_number + 1))? as usize
            } else {
                idxt_at
            };
            if end <= start || end > record.len() {
                continue;
            }
            if let Some(entry) = parse_index_entry(&record[start..end], &tagx, control_byte_count) {
                entries.push(entry);
            }
        }
    }

    Ok(Index { entries, cncx })
}

struct Tagx {
    tag: u32,
    values: u32,
    mask: u8,
    end: bool,
}

fn parse_index_entry(entry: &[u8], tagx: &[Tagx], control_byte_count: usize) -> Option<IndexEntry> {
    let mut at = 0usize;
    let ident_len = *entry.first()? as usize;
    at += 1;
    if at + ident_len > entry.len() {
        return None;
    }
    let ident = entry[at..at + ident_len].to_vec();
    at += ident_len;
    if at + control_byte_count > entry.len() {
        return None;
    }
    let control: Vec<u8> = entry[at..at + control_byte_count].to_vec();
    at += control_byte_count;

    let mut tags: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut control_index = 0usize;
    for tagx_entry in tagx {
        if control_index >= control_byte_count {
            break;
        }
        if tagx_entry.end {
            control_index += 1;
            continue;
        }
        let masked = control[control_index] & tagx_entry.mask;
        if masked == 0 {
            continue;
        }
        let values = if masked == tagx_entry.mask && tagx_entry.mask.count_ones() > 1 {
            // A vwi byte length follows, then that many bytes of values.
            let byte_length = read_vwi(entry, &mut at)? as usize;
            let mut consumed = 0usize;
            let mut values = Vec::new();
            while consumed < byte_length {
                let before = at;
                values.push(read_vwi(entry, &mut at)?);
                consumed += at - before;
            }
            values
        } else {
            let count = if tagx_entry.mask.count_ones() == 1 {
                1
            } else {
                let mut mask = tagx_entry.mask;
                let mut count = masked;
                while mask & 1 == 0 {
                    mask >>= 1;
                    count >>= 1;
                }
                count
            };
            let total = (count as usize).saturating_mul(tagx_entry.values.max(1) as usize);
            let mut values = Vec::with_capacity(total);
            for _ in 0..total {
                values.push(read_vwi(entry, &mut at)?);
            }
            values
        };
        tags.entry(tagx_entry.tag).or_default().extend(values);
    }
    Some(IndexEntry { ident, tags })
}

/// Forward variable-width integer: 7 bits per byte, big-endian, last byte's high
/// bit is set.
pub fn read_vwi(data: &[u8], at: &mut usize) -> Option<u32> {
    let mut value: u32 = 0;
    loop {
        let byte = *data.get(*at)?;
        *at += 1;
        let part = (byte & 0x7F) as u32;
        if value > (u32::MAX - part) >> 7 {
            return None;
        }
        value = (value << 7) | part;
        if byte & 0x80 != 0 {
            return Some(value);
        }
    }
}

/// True when the record looks like an image, returning its extension.
pub fn image_extension(data: &[u8]) -> Option<&'static str> {
    if data.len() < 8 {
        return None;
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if data.starts_with(b"\x89PNG") {
        return Some("png");
    }
    if data.starts_with(b"GIF8") {
        return Some("gif");
    }
    if data.starts_with(b"BM") {
        return Some("bmp");
    }
    if data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// True when the record looks like an embedded font (TTF, OTF, TTC).
///
/// KF8 books store their fonts as ordinary resource records next to the
/// images, and the stylesheet addresses them with
/// `url(kindle:embed:N?mime=application/x-font-ttf)`, so the same index that
/// names an image names a font.
pub fn is_font_record(data: &[u8]) -> bool {
    data.starts_with(&[0x00, 0x01, 0x00, 0x00])
        || data.starts_with(b"OTTO")
        || data.starts_with(b"ttcf")
        || data.starts_with(b"true")
        || data.starts_with(b"typ1")
}

/// Record types that live in the resource area but are not images.
pub fn is_known_non_image_record(data: &[u8]) -> bool {
    data.starts_with(b"FLIS")
        || data.starts_with(b"FCIS")
        || data.starts_with(b"SRCS")
        || data.starts_with(b"FDST")
        || data.starts_with(b"DATP")
        || data.starts_with(b"AUDI")
        || data.starts_with(b"VIDE")
        || data.starts_with(b"RESC")
        || data.starts_with(b"BOUN")
        || data.starts_with(b"PAGE")
        || data.starts_with(b"CMET")
        || data.starts_with(b"FONT")
        || data.starts_with(b"CONT")
        || data.starts_with(b"CRES")
        || data.starts_with(&[0xE9, 0x8E, 0x0D, 0x0A])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palm_doc_round_trip() {
        // 1..=8: literal run, 9..=0x7F: single byte, 0xC0..: space + char,
        // 0x80..=0xBF: back reference.
        // "abc" + "x" + space/`A` + a 3-byte copy at distance 1.
        let compressed = vec![3, b'a', b'b', b'c', b'x', 0xC1, 0x80, 0x08];
        assert_eq!(palm_doc_decompress(&compressed), b"abcx AAAA".to_vec());
    }

    #[test]
    fn vwi_decodes() {
        let mut at = 0;
        // 0x8A has the high bit set, so it is the last byte: value 10.
        assert_eq!(read_vwi(&[0x8A], &mut at), Some(10));
        at = 0;
        assert_eq!(read_vwi(&[0x01, 0x8A], &mut at), Some(138));
    }

    #[test]
    fn cp1252_maps_smart_quotes() {
        assert_eq!(cp1252(&[0x93, b'a', 0x94]), "\u{201C}a\u{201D}");
    }

    #[test]
    fn font_records_are_recognised_by_their_magic() {
        // A TrueType face starts with the version tag; CFF-based faces and
        // collections have their own.
        assert!(is_font_record(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x10]));
        assert!(is_font_record(b"OTTO\x00\x0a\x00\x80"));
        assert!(is_font_record(b"ttcf\x00\x01\x00\x00"));
        assert!(is_font_record(b"true\x00\x05\x00\x00"));
        // Images and record types that only look like resources are not fonts.
        assert!(!is_font_record(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]));
        assert!(!is_font_record(b"FLIS\x00\x00\x00\x08"));
        assert!(!is_font_record(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_font_record(b""));
    }

    #[test]
    fn trailing_entries_are_stripped() {
        // flag bit 0: one trailing byte whose low two bits say how many
        // multibyte bytes follow it.
        let data = b"html\x00\x00";
        assert_eq!(strip_trailing_entries(data, 0), &data[..]);
        // 0x00 low bits 0 -> one byte removed in total.
        assert_eq!(strip_trailing_entries(&b"html\x00"[..], 1), b"html");
    }

    #[test]
    fn a_mapped_file_yields_the_same_records_as_owned_bytes() {
        // `open` maps the file instead of reading it; the record table and the
        // records themselves must come out identical.
        let mut data = vec![0u8; PDB_HEADER_LEN];
        data[..4].copy_from_slice(b"book");
        data[60..68].copy_from_slice(b"BOOKMOBI");
        data[76..78].copy_from_slice(&2u16.to_be_bytes());
        let first_record = PDB_HEADER_LEN + 2 * 8;
        data.resize(first_record, 0);
        for (index, offset) in [first_record, first_record + 3].into_iter().enumerate() {
            let at = PDB_HEADER_LEN + index * 8;
            data[at..at + 4].copy_from_slice(&(offset as u32).to_be_bytes());
        }
        data.extend_from_slice(b"abc");
        data.extend_from_slice(b"defg");

        let path = write_temp("mapped.pdb", &data);
        let mapped = Pdb::open(&path).expect("open mapped");
        let owned = Pdb::from_bytes(data).expect("open owned");
        assert_eq!(mapped.name, owned.name);
        assert_eq!(mapped.book_type, owned.book_type);
        assert_eq!(mapped.records(), owned.records());
        for index in 0..owned.records() {
            assert_eq!(mapped.record(index), owned.record(index));
        }
        assert_eq!(mapped.record(1), Some(&b"defg"[..]));
        drop(mapped);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resource_ranges_slice_the_backing_bytes() {
        let mut data = vec![0u8; PDB_HEADER_LEN];
        data[60..68].copy_from_slice(b"BOOKMOBI");
        data[76..78].copy_from_slice(&2u16.to_be_bytes());
        let first_record = PDB_HEADER_LEN + 2 * 8;
        data.resize(first_record, 0);
        for (index, offset) in [first_record, first_record + 5].into_iter().enumerate() {
            let at = PDB_HEADER_LEN + index * 8;
            data[at..at + 4].copy_from_slice(&(offset as u32).to_be_bytes());
        }
        data.extend_from_slice(b"abcde");
        data.extend_from_slice(b"fghij");

        let pdb = Pdb::from_bytes(data).expect("pdb");
        let ranges: Vec<ResourceRange> = (0..pdb.records())
            .map(|index| {
                (
                    format!("image-{index}"),
                    pdb.record_range(index).expect("range"),
                )
            })
            .collect();
        let mut resources = BookResources::new(pdb.into_backing());
        for (name, range) in ranges {
            resources.insert_range(name, range);
        }
        resources.insert_owned("generated", Arc::from(&b"svg"[..]));

        assert_eq!(resources.get("image-0"), Some(&b"abcde"[..]));
        assert_eq!(resources.get("image-1"), Some(&b"fghij"[..]));
        assert_eq!(resources.get("generated"), Some(&b"svg"[..]));
        assert_eq!(resources.get("missing"), None);
        assert!(resources.contains_key("image-1"));
        assert_eq!(resources.len(), 3);
        assert_eq!(resources.bytes(), 13);
        assert!(!resources.is_mapped());
        // Dropping pages is a no-op for owned bytes.
        resources.drop_pages();
        assert_eq!(resources.get("image-0"), Some(&b"abcde"[..]));
    }

    fn write_temp(name: &str, data: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("cre-pdb-{}-{name}", std::process::id()));
        std::fs::write(&path, data).expect("write temp");
        path
    }
}
