//! Builds tiny TrueType fonts in memory for tests, so colour-font and glyph-scan
//! behaviour can be exercised without committing (or licensing) real font files.
//!
//! Each font maps a few characters to glyphs that are an outline (a filled square),
//! empty (no outline at all), or a CBDT colour bitmap (a PNG). COLR/CPAL and SVG tables
//! can be added: every *empty* glyph then gets a COLR base-glyph record (one layer) or an
//! SVG document, so it is a colour glyph without an outline - the same as in a real
//! COLR/SVG-only emoji font as far as dak is concerned.

#![allow(dead_code)]

/// What one mapped character's glyph contains.
#[derive(Clone)]
pub enum Glyph {
    /// A filled square outline (100..900 x 0..800 font units).
    Square,
    /// A filled square spanning the whole em box vertically, like a CJK ideograph
    /// (50..950 x -110..870 font units, inside Noto Sans CJK's 880/-120 em box).
    FullSquare,
    /// No outline and no bitmap.
    Empty,
    /// No outline; a CBDT colour bitmap holding these PNG bytes, declared `size` px square.
    Png(Vec<u8>, u8),
    /// No outline; a CBDT raw premultiplied-BGRA bitmap (image format 1, 32 bits per
    /// pixel) of `size` x `size` pixels given row by row.
    Bgra(Vec<u8>, u8),
}

/// Extra colour tables to add to a built font.
#[derive(Clone, Copy, Default)]
pub struct ColourTables {
    /// Add minimal `COLR` + `CPAL` tables.
    pub colr: bool,
    /// Add a minimal `SVG ` table.
    pub svg: bool,
}

/// Appends a big-endian u16.
fn u16be(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Appends a big-endian i16.
fn i16be(out: &mut Vec<u8>, value: i16) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Appends a big-endian u32.
fn u32be(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// A solid `size` x `size` PNG of one RGBA colour.
pub fn solid_png(size: u32, rgba: [u8; 4]) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(size, size, image::Rgba(rgba));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode PNG");
    bytes.into_inner()
}

/// The `glyf` data of a filled rectangle from (`x0`, `y0`) to (`x1`, `y1`).
fn rectangle_glyph(x0: i16, y0: i16, x1: i16, y1: i16) -> Vec<u8> {
    let mut g = Vec::new();
    i16be(&mut g, 1); // numberOfContours
    for value in [x0, y0, x1, y1] {
        i16be(&mut g, value); // xMin, yMin, xMax, yMax
    }
    u16be(&mut g, 3); // endPtsOfContours[0]
    u16be(&mut g, 0); // instructionLength
    g.extend_from_slice(&[1, 1, 1, 1]); // flags: on-curve, 16-bit deltas
    for dx in [x0, 0, x1 - x0, 0] {
        i16be(&mut g, dx);
    }
    for dy in [y0, y1 - y0, 0, y0 - y1] {
        i16be(&mut g, dy);
    }
    g
}

/// Vertical metrics of a built font, in its 1000 units per em.
#[derive(Clone, Copy)]
pub struct VerticalMetrics {
    /// `hhea` ascender (top of the line box).
    pub ascender: i16,
    /// `hhea` descender (bottom of the line box, negative).
    pub descender: i16,
    /// OS/2 typographic ascender/descender; `None` builds no OS/2 table.
    pub typo: Option<(i16, i16)>,
}

impl Default for VerticalMetrics {
    /// A line box of exactly one em (800 above, 200 below the baseline), no OS/2 table.
    fn default() -> Self {
        Self {
            ascender: 800,
            descender: -200,
            typo: None,
        }
    }
}

/// The vertical metrics of Noto Sans CJK: a line box much taller than the em
/// (1160/-288) and typographic metrics spanning exactly one em (880/-120).
pub const CJK_METRICS: VerticalMetrics = VerticalMetrics {
    ascender: 1160,
    descender: -288,
    typo: Some((880, -120)),
};

/// Builds a font mapping each `(char, Glyph)` (glyph ids 1.. in order; 0 is an empty
/// `.notdef`), plus the requested colour tables. Returns the font file bytes.
pub fn build_font(glyphs: &[(char, Glyph)], colour: ColourTables) -> Vec<u8> {
    build_font_with_metrics(glyphs, colour, VerticalMetrics::default())
}

/// [`build_font`] with the given vertical metrics.
pub fn build_font_with_metrics(
    glyphs: &[(char, Glyph)],
    colour: ColourTables,
    metrics: VerticalMetrics,
) -> Vec<u8> {
    let num_glyphs = glyphs.len() as u16 + 1;
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();

    // head
    let mut head = Vec::new();
    u32be(&mut head, 0x0001_0000); // version
    u32be(&mut head, 0x0001_0000); // fontRevision
    u32be(&mut head, 0); // checkSumAdjustment
    u32be(&mut head, 0x5F0F_3CF5); // magicNumber
    u16be(&mut head, 0); // flags
    u16be(&mut head, 1000); // unitsPerEm
    head.extend_from_slice(&[0; 16]); // created, modified
    for value in [0, -200, 1000, 800] {
        i16be(&mut head, value); // xMin, yMin, xMax, yMax
    }
    u16be(&mut head, 0); // macStyle
    u16be(&mut head, 8); // lowestRecPPEM
    i16be(&mut head, 2); // fontDirectionHint
    i16be(&mut head, 1); // indexToLocFormat: long
    i16be(&mut head, 0); // glyphDataFormat
    tables.push((*b"head", head));

    // hhea
    let mut hhea = Vec::new();
    u32be(&mut hhea, 0x0001_0000);
    i16be(&mut hhea, metrics.ascender);
    i16be(&mut hhea, metrics.descender);
    i16be(&mut hhea, 0); // lineGap
    u16be(&mut hhea, 1000); // advanceWidthMax
    for value in [0, 0, 1000, 1, 0, 0, 0, 0, 0, 0, 0] {
        i16be(&mut hhea, value); // bearings, extent, caret, reserved, metricDataFormat
    }
    u16be(&mut hhea, num_glyphs); // numberOfHMetrics
    tables.push((*b"hhea", hhea));

    // OS/2 version 0 (78 bytes): only the typographic ascender/descender matter here.
    if let Some((typo_ascender, typo_descender)) = metrics.typo {
        let mut os2 = vec![0u8; 78];
        os2[68..70].copy_from_slice(&typo_ascender.to_be_bytes());
        os2[70..72].copy_from_slice(&typo_descender.to_be_bytes());
        tables.push((*b"OS/2", os2));
    }

    // maxp (version 1.0; limits left at zero)
    let mut maxp = Vec::new();
    u32be(&mut maxp, 0x0001_0000);
    u16be(&mut maxp, num_glyphs);
    maxp.extend_from_slice(&[0; 26]);
    tables.push((*b"maxp", maxp));

    // hmtx: every glyph 1000 units wide
    let mut hmtx = Vec::new();
    for _ in 0..num_glyphs {
        u16be(&mut hmtx, 1000);
        i16be(&mut hmtx, 0);
    }
    tables.push((*b"hmtx", hmtx));

    // cmap: one Windows Unicode full-repertoire format 12 subtable
    let mut mapped: Vec<(u32, u16)> = glyphs
        .iter()
        .enumerate()
        .map(|(index, (ch, _))| (*ch as u32, index as u16 + 1))
        .collect();
    mapped.sort();
    let mut cmap = Vec::new();
    u16be(&mut cmap, 0); // version
    u16be(&mut cmap, 1); // numTables
    u16be(&mut cmap, 3); // platform: Windows
    u16be(&mut cmap, 10); // encoding: Unicode full repertoire
    u32be(&mut cmap, 12); // offset
    u16be(&mut cmap, 12); // format
    u16be(&mut cmap, 0); // reserved
    u32be(&mut cmap, 16 + 12 * mapped.len() as u32); // length
    u32be(&mut cmap, 0); // language
    u32be(&mut cmap, mapped.len() as u32);
    for (code, id) in &mapped {
        u32be(&mut cmap, *code);
        u32be(&mut cmap, *code);
        u32be(&mut cmap, u32::from(*id));
    }
    tables.push((*b"cmap", cmap));

    // glyf + loca
    let mut glyf = Vec::new();
    let mut loca = Vec::new();
    // numGlyphs + 1 offsets: glyph i spans loca[i]..loca[i + 1].
    u32be(&mut loca, 0); // start of .notdef
    u32be(&mut loca, 0); // end of .notdef (empty)
    for (_, glyph) in glyphs {
        match glyph {
            Glyph::Square => glyf.extend(rectangle_glyph(100, 0, 900, 800)),
            Glyph::FullSquare => glyf.extend(rectangle_glyph(50, -110, 950, 870)),
            _ => {}
        }
        u32be(&mut loca, glyf.len() as u32);
    }
    tables.push((*b"glyf", glyf));
    tables.push((*b"loca", loca));

    // CBDT + CBLC: one index subtable per bitmap glyph, so glyphs without a bitmap are
    // not covered by any subtable (a reader then finds no image for them).
    // (glyph id, image data, size, whether the data is PNG rather than raw BGRA)
    let bitmaps: Vec<(u16, &[u8], u8, bool)> = glyphs
        .iter()
        .enumerate()
        .filter_map(|(index, (_, glyph))| match glyph {
            Glyph::Png(png, size) => Some((index as u16 + 1, png.as_slice(), *size, true)),
            Glyph::Bgra(bgra, size) => Some((index as u16 + 1, bgra.as_slice(), *size, false)),
            _ => None,
        })
        .collect();
    if !bitmaps.is_empty() {
        let mut cbdt = Vec::new();
        u16be(&mut cbdt, 3);
        u16be(&mut cbdt, 0);
        // (glyph id, offset in CBDT, record length, image format)
        let mut records = Vec::new();
        for (id, data, size, is_png) in &bitmaps {
            let start = cbdt.len() as u32;
            cbdt.extend_from_slice(&[*size, *size, 0, *size, *size]); // smallGlyphMetrics
            if *is_png {
                u32be(&mut cbdt, data.len() as u32);
            }
            cbdt.extend_from_slice(data);
            // 17: small metrics + PNG; 1: small metrics + byte-aligned bitmap data.
            let format = if *is_png { 17 } else { 1 };
            records.push((*id, start, cbdt.len() as u32 - start, format));
        }
        let count = records.len() as u32;
        let array_offset = 8 + 48;
        let mut array = Vec::new();
        let mut subtables = Vec::new();
        for (id, offset, length, format) in &records {
            u16be(&mut array, *id);
            u16be(&mut array, *id);
            u32be(&mut array, count * 8 + subtables.len() as u32);
            u16be(&mut subtables, 1); // indexFormat
            u16be(&mut subtables, *format); // imageFormat
            u32be(&mut subtables, *offset); // imageDataOffset
            u32be(&mut subtables, 0);
            u32be(&mut subtables, *length);
        }
        let size = bitmaps[0].2;
        let mut cblc = Vec::new();
        u16be(&mut cblc, 3);
        u16be(&mut cblc, 0);
        u32be(&mut cblc, 1); // numSizes
        u32be(&mut cblc, array_offset); // indexSubTableArrayOffset
        u32be(&mut cblc, (array.len() + subtables.len()) as u32); // indexTablesSize
        u32be(&mut cblc, count); // numberOfIndexSubTables
        u32be(&mut cblc, 0); // colorRef
        let metrics = [size, 0, size, 0, 0, 0, 0, 0, size, 0, 0, 0];
        cblc.extend_from_slice(&metrics); // hori
        cblc.extend_from_slice(&metrics); // vert
        u16be(&mut cblc, bitmaps.first().unwrap().0); // startGlyphIndex
        u16be(&mut cblc, bitmaps.last().unwrap().0); // endGlyphIndex
        cblc.extend_from_slice(&[size, size, 32, 1]); // ppemX, ppemY, bitDepth, flags
        cblc.extend(array);
        cblc.extend(subtables);
        tables.push((*b"CBDT", cbdt));
        tables.push((*b"CBLC", cblc));
    }

    let empty_ids: Vec<u16> = glyphs
        .iter()
        .enumerate()
        .filter(|(_, (_, glyph))| matches!(glyph, Glyph::Empty))
        .map(|(index, _)| index as u16 + 1)
        .collect();
    if colour.colr {
        // Version 0: one base glyph record per empty glyph, all sharing one layer
        // (glyph 0 in palette colour 0).
        let records = empty_ids.len() as u16;
        let mut colr = Vec::new();
        u16be(&mut colr, 0); // version 0
        u16be(&mut colr, records); // numBaseGlyphRecords
        u32be(&mut colr, 14); // baseGlyphRecordsOffset
        u32be(&mut colr, 14 + 6 * u32::from(records)); // layerRecordsOffset
        u16be(&mut colr, 1); // numLayerRecords
        for id in &empty_ids {
            u16be(&mut colr, *id); // glyphID
            u16be(&mut colr, 0); // firstLayerIndex
            u16be(&mut colr, 1); // numLayers
        }
        u16be(&mut colr, 0); // layer glyphID
        u16be(&mut colr, 0); // paletteIndex
        tables.push((*b"COLR", colr));
        let mut cpal = Vec::new();
        for value in [0u16, 0, 1, 0] {
            u16be(&mut cpal, value); // version, entries, palettes, colour records
        }
        u32be(&mut cpal, 14); // colorRecordsArrayOffset
        u16be(&mut cpal, 0); // colorRecordIndices[0]
        tables.push((*b"CPAL", cpal));
    }
    if colour.svg {
        // One tiny SVG document per empty glyph.
        let document = b"<svg xmlns='http://www.w3.org/2000/svg'/>";
        let entries = empty_ids.len() as u32;
        let list_header = 2 + 12 * entries;
        let mut svg = Vec::new();
        u16be(&mut svg, 0); // version
        u32be(&mut svg, 10); // offsetToSVGDocumentList
        u32be(&mut svg, 0); // reserved
        u16be(&mut svg, entries as u16); // numEntries
        for (index, id) in empty_ids.iter().enumerate() {
            u16be(&mut svg, *id); // startGlyphID
            u16be(&mut svg, *id); // endGlyphID
            u32be(&mut svg, list_header + index as u32 * document.len() as u32); // svgDocOffset
            u32be(&mut svg, document.len() as u32); // svgDocLength
        }
        for _ in &empty_ids {
            svg.extend_from_slice(document);
        }
        tables.push((*b"SVG ", svg));
    }

    // Table directory (records sorted by tag), then the 4-byte aligned table data.
    tables.sort_by_key(|table| table.0);
    let mut font = Vec::new();
    u32be(&mut font, 0x0001_0000);
    u16be(&mut font, tables.len() as u16);
    font.extend_from_slice(&[0; 6]); // searchRange etc., unused by readers here
    let mut offset = 12 + 16 * tables.len();
    let mut data = Vec::new();
    for (tag, bytes) in &tables {
        font.extend_from_slice(tag);
        u32be(&mut font, 0); // checksum, not verified
        u32be(&mut font, offset as u32);
        u32be(&mut font, bytes.len() as u32);
        data.extend_from_slice(bytes);
        while data.len() % 4 != 0 {
            data.push(0);
        }
        offset = 12 + 16 * tables.len() + data.len();
    }
    font.extend(data);
    font
}

/// Writes `bytes` to a fresh file named `name` in a unique temp dir, returning its path.
pub fn write_font(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = super::temp_dir().join(name);
    std::fs::write(&path, bytes).expect("write test font");
    path
}
