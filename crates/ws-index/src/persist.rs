//! On-disk index cache. A compact binary format so warm startups are near
//! instant: load the arena + entries, rebuild the FRN map, then catch up on
//! whatever changed via the USN journal.

use crate::{Entry, Snapshot};
use anyhow::{bail, Result};
use rustc_hash::FxHashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"WSIX";
const VERSION: u32 = 1;
/// Packed on-disk size of one Entry: frn(8)+parent(8)+off(4)+len(2)+attr(4)
/// +size(8)+mtime(8)+ctime(8).
const ENTRY_BYTES: usize = 50;

/// A volume as loaded from the cache, including where its journal left off.
pub struct LoadedVolume {
    pub snapshot: Snapshot,
    pub is_ntfs: bool,
    pub journal_id: u64,
    pub next_usn: i64,
}

/// One volume's data to persist.
pub struct SaveVolume<'a> {
    pub snapshot: &'a Snapshot,
    pub is_ntfs: bool,
    pub journal_id: u64,
    pub next_usn: i64,
}

pub fn save(path: &Path, volumes: &[SaveVolume]) -> Result<()> {
    let f = std::fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, f);
    w.write_all(MAGIC)?;
    wu32(&mut w, VERSION)?;
    wu32(&mut w, volumes.len() as u32)?;
    for v in volumes {
        let s = v.snapshot;
        w.write_all(&[s.volume_letter as u8, v.is_ntfs as u8, s.meta_ready as u8])?;
        wu64(&mut w, v.journal_id)?;
        wi64(&mut w, v.next_usn)?;
        let rp = s.root_prefix.as_bytes();
        wu32(&mut w, rp.len() as u32)?;
        w.write_all(rp)?;
        wu64(&mut w, s.names.len() as u64)?;
        w.write_all(&s.names)?;
        wu64(&mut w, s.entries.len() as u64)?;
        // Pack entries into a byte buffer, then one write.
        let mut buf = Vec::with_capacity(s.entries.len() * ENTRY_BYTES);
        for e in &s.entries {
            buf.extend_from_slice(&e.frn.to_le_bytes());
            buf.extend_from_slice(&e.parent_frn.to_le_bytes());
            buf.extend_from_slice(&e.name_off.to_le_bytes());
            buf.extend_from_slice(&e.name_len.to_le_bytes());
            buf.extend_from_slice(&e.attributes.to_le_bytes());
            buf.extend_from_slice(&e.size.to_le_bytes());
            buf.extend_from_slice(&e.mtime.to_le_bytes());
            buf.extend_from_slice(&e.ctime.to_le_bytes());
        }
        w.write_all(&buf)?;
    }
    w.flush()?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Vec<LoadedVolume>> {
    let f = std::fs::File::open(path)?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("bad cache magic");
    }
    if ru32(&mut r)? != VERSION {
        bail!("cache version mismatch");
    }
    let vcount = ru32(&mut r)?;
    let mut out = Vec::with_capacity(vcount as usize);
    for _ in 0..vcount {
        let mut hdr = [0u8; 3];
        r.read_exact(&mut hdr)?;
        let letter = hdr[0] as char;
        let is_ntfs = hdr[1] != 0;
        let meta_ready = hdr[2] != 0;
        let journal_id = ru64(&mut r)?;
        let next_usn = ri64(&mut r)?;
        let rp_len = ru32(&mut r)? as usize;
        let mut rp = vec![0u8; rp_len];
        r.read_exact(&mut rp)?;
        let root_prefix = String::from_utf8_lossy(&rp).into_owned();
        let names_len = ru64(&mut r)? as usize;
        let mut names = vec![0u8; names_len];
        r.read_exact(&mut names)?;
        let ecount = ru64(&mut r)? as usize;
        let mut ebuf = vec![0u8; ecount * ENTRY_BYTES];
        r.read_exact(&mut ebuf)?;
        let mut entries = Vec::with_capacity(ecount);
        let mut frn_index = FxHashMap::with_capacity_and_hasher(ecount, Default::default());
        for i in 0..ecount {
            let b = &ebuf[i * ENTRY_BYTES..i * ENTRY_BYTES + ENTRY_BYTES];
            let e = Entry {
                frn: u64::from_le_bytes(b[0..8].try_into().unwrap()),
                parent_frn: u64::from_le_bytes(b[8..16].try_into().unwrap()),
                name_off: u32::from_le_bytes(b[16..20].try_into().unwrap()),
                name_len: u16::from_le_bytes(b[20..22].try_into().unwrap()),
                attributes: u32::from_le_bytes(b[22..26].try_into().unwrap()),
                size: u64::from_le_bytes(b[26..34].try_into().unwrap()),
                mtime: i64::from_le_bytes(b[34..42].try_into().unwrap()),
                ctime: i64::from_le_bytes(b[42..50].try_into().unwrap()),
            };
            frn_index.insert(e.frn, i as u32);
            entries.push(e);
        }
        out.push(LoadedVolume {
            snapshot: Snapshot {
                volume_letter: letter,
                root_prefix,
                names,
                entries,
                frn_index,
                meta_ready,
            },
            is_ntfs,
            journal_id,
            next_usn,
        });
    }
    Ok(out)
}

/// Default cache location under %LOCALAPPDATA%\WinSearch\index.bin.
pub fn default_cache_path() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("TEMP"))
        .unwrap_or_else(|_| ".".into());
    let dir = std::path::Path::new(&base).join("WinSearch");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("index.bin")
}

fn wu32<W: Write>(w: &mut W, v: u32) -> Result<()> {
    w.write_all(&v.to_le_bytes())?;
    Ok(())
}
fn wu64<W: Write>(w: &mut W, v: u64) -> Result<()> {
    w.write_all(&v.to_le_bytes())?;
    Ok(())
}
fn wi64<W: Write>(w: &mut W, v: i64) -> Result<()> {
    w.write_all(&v.to_le_bytes())?;
    Ok(())
}
fn ru32<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn ru64<R: Read>(r: &mut R) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn ri64<R: Read>(r: &mut R) -> Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}
