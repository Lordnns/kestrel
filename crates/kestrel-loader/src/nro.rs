//! Homebrew executables (NRO), the open format homebrew toolchains produce.
//!
//! Layout from Switchbrew's public "NRO" page: a 0x10-byte start block, the
//! 0x70-byte header at 0x10 (magic `NRO0`), then the image itself. Segment
//! offsets are memory offsets from the load base, and the file is the
//! memory image: the header sits at the start of `.text`, and `.bss`
//! follows `.data`. An optional asset section (`ASET`) after the image holds
//! the icon, the NACP metadata and a RomFS.

use crate::{rd32, rd64, LoadError, Loaded};
use kestrel_core::{Memory, Perm, PAGE_SIZE};
use std::ops::Range;

const HEADER_END: usize = 0x80;
/// Header flag bit 1: segments are compressed (newer system versions).
const FLAG_COMPRESSED: u32 = 1 << 1;
const ASSET_HEADER_SIZE: usize = 0x38;

/// A loaded NRO.
#[derive(Debug, Clone)]
pub struct Nro {
    pub image: Loaded,
    /// The module (build) ID from the header.
    pub build_id: [u8; 32],
    /// The asset section, if the file has one.
    pub assets: Option<Assets>,
}

/// File ranges of the assets; `None` where the section is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assets {
    pub icon: Option<Range<usize>>,
    pub nacp: Option<Range<usize>>,
    pub romfs: Option<Range<usize>>,
}

/// Whether `file` starts like an NRO.
pub fn is_nro(file: &[u8]) -> bool {
    file.get(0x10..0x14) == Some(b"NRO0")
}

fn page_up(v: u64) -> u64 {
    (v + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Map the NRO in `file` at `base` (page aligned): `.text` read-execute,
/// `.rodata` read-only, `.data` and `.bss` read-write, `.bss` zeroed.
/// The entry point is the start of `.text`.
pub fn load_nro(file: &[u8], base: u64, mem: &mut Memory) -> Result<Nro, LoadError> {
    if !is_nro(file) {
        return Err(LoadError::InvalidNro("no NRO0 magic at offset 0x10"));
    }
    if file.len() < HEADER_END {
        return Err(LoadError::InvalidNro("file is shorter than the header"));
    }
    let size = rd32(file, 0x18)? as usize;
    if size < HEADER_END || size > file.len() {
        return Err(LoadError::InvalidNro("header size is outside the file"));
    }
    if rd32(file, 0x1c)? & FLAG_COMPRESSED != 0 {
        return Err(LoadError::UnsupportedNro("compressed segments"));
    }
    let seg = |i: usize| -> Result<(u64, u64), LoadError> {
        Ok((
            rd32(file, 0x20 + 8 * i)? as u64,
            rd32(file, 0x24 + 8 * i)? as u64,
        ))
    };
    let (text, ro, data) = (seg(0)?, seg(1)?, seg(2)?);
    let bss = rd32(file, 0x38)? as u64;
    if text.0 != 0 {
        return Err(LoadError::InvalidNro(".text does not start at offset 0"));
    }
    // Segments are page aligned and in order: text, rodata, data (+ bss).
    for ((off, sz), next) in [(text, ro.0), (ro, data.0)] {
        if off % PAGE_SIZE != 0 || page_up(off + sz) > next {
            return Err(LoadError::InvalidNro(
                "segments overlap or are not page aligned",
            ));
        }
    }
    if data.0 % PAGE_SIZE != 0 || data.0 + data.1 > size as u64 {
        return Err(LoadError::InvalidNro("segments extend past the image"));
    }
    if text.1 == 0 {
        return Err(LoadError::InvalidNro(".text is empty"));
    }
    let end = page_up(data.0 + data.1 + bss);
    let regions = [
        (text.0, page_up(text.1), Perm::RX, "nro:text"),
        (ro.0, page_up(ro.0 + ro.1) - ro.0, Perm::R, "nro:rodata"),
        (data.0, end - data.0, Perm::RW, "nro:data"),
    ];
    for (off, len, perm, name) in regions {
        if len != 0 {
            mem.map(base + off, len, perm, name)?;
        }
    }
    for (off, sz) in [text, ro, data] {
        mem.poke_bytes(base + off, &file[off as usize..(off + sz) as usize])?;
    }
    let mut build_id = [0; 32];
    build_id.copy_from_slice(&file[0x40..0x60]);
    Ok(Nro {
        image: Loaded {
            entry: base,
            image_start: base,
            image_end: base + end,
        },
        build_id,
        assets: assets(file, size)?,
    })
}

/// Parse the asset section at `at`, if there is one.
fn assets(file: &[u8], at: usize) -> Result<Option<Assets>, LoadError> {
    if file.len() == at {
        return Ok(None);
    }
    if file.get(at..at + 4) != Some(b"ASET") {
        return Err(LoadError::InvalidNro(
            "data after the image is not an ASET section",
        ));
    }
    if file.len() < at + ASSET_HEADER_SIZE {
        return Err(LoadError::InvalidNro("asset header is truncated"));
    }
    if rd32(file, at + 4)? != 0 {
        return Err(LoadError::UnsupportedNro(
            "asset section version other than 0",
        ));
    }
    let section = |i: usize| -> Result<Option<Range<usize>>, LoadError> {
        let off = rd64(file, at + 8 + 16 * i)?;
        let len = rd64(file, at + 16 + 16 * i)?;
        if len == 0 {
            return Ok(None);
        }
        let start = (at as u64).checked_add(off);
        match start.and_then(|s| s.checked_add(len).map(|e| (s, e))) {
            Some((s, e)) if e <= file.len() as u64 => Ok(Some(s as usize..e as usize)),
            _ => Err(LoadError::InvalidNro("an asset extends past the file")),
        }
    };
    Ok(Some(Assets {
        icon: section(0)?,
        nacp: section(1)?,
        romfs: section(2)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x800_0000;

    fn put32(f: &mut [u8], at: usize, v: u32) {
        f[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// A minimal NRO laid out by hand from the Switchbrew table: one page
    /// each of text, rodata and data, then 0x1800 bytes of bss.
    fn nro() -> Vec<u8> {
        let mut f = vec![0u8; 0x3000];
        f[0x10..0x14].copy_from_slice(b"NRO0");
        put32(&mut f, 0x18, 0x3000);
        for (i, (off, size)) in [(0, 0x1000), (0x1000, 0x1000), (0x2000, 0x1000)]
            .into_iter()
            .enumerate()
        {
            put32(&mut f, 0x20 + 8 * i, off);
            put32(&mut f, 0x24 + 8 * i, size);
        }
        put32(&mut f, 0x38, 0x1800);
        f[0x40] = 0xab;
        f[0x100] = 0x11; // text
        f[0x1000] = 0x22; // rodata
        f[0x2ff8] = 0x33; // last data byte
        f
    }

    #[test]
    fn maps_segments_with_their_permissions() {
        let mut mem = Memory::new();
        let n = load_nro(&nro(), BASE, &mut mem).unwrap();
        assert_eq!(n.image.entry, BASE);
        assert_eq!(n.image.image_end, BASE + 0x5000);
        assert_eq!(n.build_id[0], 0xab);
        assert!(n.assets.is_none());
        let regions: Vec<_> = mem.regions().map(|(b, s, p, _)| (b, s, p)).collect();
        assert_eq!(
            regions,
            [
                (BASE, 0x1000, Perm::RX),
                (BASE + 0x1000, 0x1000, Perm::R),
                (BASE + 0x2000, 0x3000, Perm::RW),
            ]
        );
        let byte = |a| mem.read_vec(a, 1).unwrap()[0];
        assert_eq!(byte(BASE + 0x10), b'N');
        assert_eq!(byte(BASE + 0x100), 0x11);
        assert_eq!(byte(BASE + 0x1000), 0x22);
        assert_eq!(byte(BASE + 0x2ff8), 0x33);
        // bss: zeroed, writable, ends inside the last page.
        assert_eq!(byte(BASE + 0x3000), 0);
        assert_eq!(byte(BASE + 0x47ff), 0);
    }

    #[test]
    fn reads_the_asset_section() {
        let mut f = nro();
        let mut aset = vec![0u8; ASSET_HEADER_SIZE];
        aset[0..4].copy_from_slice(b"ASET");
        // icon: 4 bytes at +0x38; nacp: empty; romfs: 2 bytes at +0x3c.
        aset[0x08..0x10].copy_from_slice(&0x38u64.to_le_bytes());
        aset[0x10..0x18].copy_from_slice(&4u64.to_le_bytes());
        aset[0x28..0x30].copy_from_slice(&0x3cu64.to_le_bytes());
        aset[0x30..0x38].copy_from_slice(&2u64.to_le_bytes());
        f.extend(aset);
        f.extend([1, 2, 3, 4, 5, 6]);
        let n = load_nro(&f, BASE, &mut Memory::new()).unwrap();
        let a = n.assets.unwrap();
        assert_eq!(a.icon, Some(0x3038..0x303c));
        assert_eq!(a.nacp, None);
        assert_eq!(a.romfs, Some(0x303c..0x303e));
        assert_eq!(&f[a.romfs.unwrap()], &[5, 6]);
    }

    #[test]
    fn rejects_bad_files_with_a_reason() {
        let load = |f: &[u8]| {
            load_nro(f, BASE, &mut Memory::new())
                .unwrap_err()
                .to_string()
        };
        assert!(!is_nro(b"\x7fELF"));
        let mut f = nro();
        put32(&mut f, 0x18, 0x4000);
        assert!(load(&f).contains("header size"));
        let mut f = nro();
        put32(&mut f, 0x1c, FLAG_COMPRESSED);
        assert!(load(&f).contains("compressed"));
        let mut f = nro();
        put32(&mut f, 0x24, 0x1800); // text runs into rodata
        assert!(load(&f).contains("overlap"));
        let mut f = nro();
        f.extend(b"junk");
        assert!(load(&f).contains("ASET"));
        let mut f = nro();
        f.extend(b"ASET");
        f.extend([0; ASSET_HEADER_SIZE - 4]);
        f[0x3008..0x3010].copy_from_slice(&0x100u64.to_le_bytes()); // icon offset
        f[0x3010..0x3018].copy_from_slice(&1u64.to_le_bytes()); // past the end
        assert!(load(&f).contains("past the file"));
    }
}
