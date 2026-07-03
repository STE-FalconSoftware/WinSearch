//! Low-level Windows NTFS access: volume handles, MFT enumeration via the USN
//! infrastructure, USN change-journal reading, and per-file metadata lookup.
//!
//! This module is the only place that talks to Win32. Everything above it works
//! with plain Rust data.
#![cfg(windows)]

use anyhow::{bail, Result};
use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_HANDLE_EOF, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, GetLogicalDrives, GetVolumeInformationW, OpenFileById,
    ReadFile, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED};

const GENERIC_READ: u32 = 0x8000_0000;

// FSCTL control codes (CTL_CODE macro pre-computed).
const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00b3;
const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;

/// Owning wrapper around a Win32 HANDLE that closes on drop.
///
/// The handle is used as a read-only "volume hint" for `OpenFileById` and for
/// `DeviceIoControl`; Windows permits concurrent use from multiple threads, so
/// we mark it `Send + Sync`. We never mutate the handle after creation.
pub struct Handle(pub HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open a raw volume handle, e.g. for letter `'C'` opens `\\.\C:`.
pub fn open_volume(letter: char) -> Result<Handle> {
    let path = wide(&format!("\\\\.\\{}:", letter));
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        let e = unsafe { GetLastError() };
        bail!("open volume {}: Win32 error {}", letter, e);
    }
    Ok(Handle(h))
}

/// Return the fixed NTFS drive letters present on the system.
pub fn ntfs_volumes() -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        if is_ntfs(letter) {
            out.push(letter);
        }
    }
    out
}

/// Report whether a drive letter is formatted NTFS.
pub fn is_ntfs(letter: char) -> bool {
    let root = wide(&format!("{}:\\", letter));
    let mut fs_name = [0u16; 32];
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return false;
    }
    let end = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    let name = String::from_utf16_lossy(&fs_name[..end]);
    name.eq_ignore_ascii_case("NTFS")
}

#[repr(C)]
struct MftEnumDataV0 {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

/// Header of a USN_RECORD_V2. The variable-length file name follows at
/// `file_name_offset` bytes from the start of the record.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UsnRecordV2 {
    pub record_length: u32,
    pub major_version: u16,
    pub minor_version: u16,
    pub file_reference_number: u64,
    pub parent_file_reference_number: u64,
    pub usn: i64,
    pub timestamp: i64,
    pub reason: u32,
    pub source_info: u32,
    pub security_id: u32,
    pub file_attributes: u32,
    pub file_name_length: u16,
    pub file_name_offset: u16,
}

// USN reason flags we care about for live updates.
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

/// A decoded record handed to enumeration/journal callbacks.
pub struct RecordView<'a> {
    pub frn: u64,
    pub parent_frn: u64,
    pub attributes: u32,
    pub reason: u32,
    pub name: &'a [u16],
}

/// Owned form of a record, for buffering journal changes past the callback.
pub struct RecordViewOwned {
    pub frn: u64,
    pub parent_frn: u64,
    pub attributes: u32,
    pub reason: u32,
    pub name: String,
}

/// Enumerate every file/dir on the volume via the MFT. Calls `cb` once per
/// record. This does not require an active change journal.
pub fn enumerate_mft<F: FnMut(RecordView)>(vol: &Handle, mut cb: F) -> Result<()> {
    let mut in_data = MftEnumDataV0 {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: i64::MAX,
    };
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB per call

    loop {
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                vol.0,
                FSCTL_ENUM_USN_DATA,
                &in_data as *const _ as *const c_void,
                std::mem::size_of::<MftEnumDataV0>() as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e == ERROR_HANDLE_EOF {
                break;
            }
            bail!("FSCTL_ENUM_USN_DATA failed: Win32 error {}", e);
        }
        if returned < 8 {
            break;
        }
        let next = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        parse_records(&buf[..returned as usize], 8, &mut cb);
        in_data.start_file_reference_number = next;
    }
    Ok(())
}

/// Parse a buffer of consecutive USN records starting at `start`.
fn parse_records<F: FnMut(RecordView)>(buf: &[u8], start: usize, cb: &mut F) {
    let mut off = start;
    while off + std::mem::size_of::<UsnRecordV2>() <= buf.len() {
        let hdr: UsnRecordV2 =
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const UsnRecordV2) };
        let rec_len = hdr.record_length as usize;
        if rec_len < std::mem::size_of::<UsnRecordV2>() || off + rec_len > buf.len() {
            break;
        }
        // Only V2 records carry 64-bit FRNs in this layout.
        if hdr.major_version == 2 {
            let name_off = off + hdr.file_name_offset as usize;
            let name_len = hdr.file_name_length as usize;
            if name_off + name_len <= buf.len() {
                let bytes = &buf[name_off..name_off + name_len];
                let name = unsafe {
                    std::slice::from_raw_parts(bytes.as_ptr() as *const u16, name_len / 2)
                };
                cb(RecordView {
                    frn: hdr.file_reference_number,
                    parent_frn: hdr.parent_file_reference_number,
                    attributes: hdr.file_attributes,
                    reason: hdr.reason,
                    name,
                });
            }
        }
        off += rec_len;
    }
}

#[repr(C)]
struct UsnJournalDataV0 {
    usn_journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
}

#[repr(C)]
struct ReadUsnJournalDataV0 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    usn_journal_id: u64,
}

/// Journal identity + current position, used to tail live changes.
#[derive(Clone, Copy)]
pub struct JournalState {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// Query the volume's USN journal for its id and current end position.
pub fn query_journal(vol: &Handle) -> Result<JournalState> {
    let mut data: UsnJournalDataV0 = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            vol.0,
            FSCTL_QUERY_USN_JOURNAL,
            null(),
            0,
            &mut data as *mut _ as *mut c_void,
            std::mem::size_of::<UsnJournalDataV0>() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        let e = unsafe { GetLastError() };
        bail!("FSCTL_QUERY_USN_JOURNAL failed: Win32 error {}", e);
    }
    Ok(JournalState {
        journal_id: data.usn_journal_id,
        next_usn: data.next_usn,
    })
}

/// Read new journal records since `state.next_usn`. Returns the decoded records
/// and the updated position. Non-blocking: returns immediately if nothing new.
pub fn read_journal<F: FnMut(RecordView)>(
    vol: &Handle,
    state: JournalState,
    mut cb: F,
) -> Result<JournalState> {
    let read = ReadUsnJournalDataV0 {
        start_usn: state.next_usn,
        reason_mask: 0xFFFF_FFFF,
        return_only_on_close: 0,
        timeout: 0,
        bytes_to_wait_for: 0,
        usn_journal_id: state.journal_id,
    };
    let mut buf = vec![0u8; 1 << 18]; // 256 KiB
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            vol.0,
            FSCTL_READ_USN_JOURNAL,
            &read as *const _ as *const c_void,
            std::mem::size_of::<ReadUsnJournalDataV0>() as u32,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        let e = unsafe { GetLastError() };
        bail!("FSCTL_READ_USN_JOURNAL failed: Win32 error {}", e);
    }
    if returned < 8 {
        return Ok(state);
    }
    let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
    parse_records(&buf[..returned as usize], 8, &mut cb);
    Ok(JournalState {
        journal_id: state.journal_id,
        next_usn: next,
    })
}

// ---------------------------------------------------------------------------
// Raw $MFT parsing (M5): read the MFT sequentially off the volume and decode
// names + sizes + timestamps in a single pass. This is far faster than opening
// a handle per file for metadata. Falls back to the USN-enum path on any error.
// ---------------------------------------------------------------------------

/// A fully-decoded MFT record: everything the index needs, in one pass.
pub struct RecordFull<'a> {
    pub frn: u64,
    pub parent_frn: u64,
    pub attributes: u32,
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub name: &'a [u16],
}

struct Geometry {
    sector_size: usize,
    cluster_size: u64,
    record_size: usize,
    mft_offset: u64,
}

/// Read `buf.len()` bytes from the volume at absolute byte `offset`. Offset and
/// length must be sector-aligned (cluster alignment satisfies this).
fn read_at(vol: &Handle, offset: u64, buf: &mut [u8]) -> Result<()> {
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.Anonymous.Anonymous.Offset = offset as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(
            vol.0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut read,
            &mut ov,
        )
    };
    if ok == 0 {
        bail!("ReadFile @ {} failed: Win32 error {}", offset, unsafe {
            GetLastError()
        });
    }
    if read as usize != buf.len() {
        bail!("short read @ {}: {} of {}", offset, read, buf.len());
    }
    Ok(())
}

#[inline]
fn ru16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
fn ru32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
fn ru64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
#[inline]
fn ri64(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn read_geometry(vol: &Handle) -> Result<Geometry> {
    let mut boot = [0u8; 512];
    read_at(vol, 0, &mut boot)?;
    let sector_size = ru16(&boot, 0x0B) as usize;
    let sectors_per_cluster = boot[0x0D] as u64;
    if sector_size == 0 || sectors_per_cluster == 0 {
        bail!("bad NTFS geometry");
    }
    let cluster_size = sector_size as u64 * sectors_per_cluster;
    let mft_lcn = ru64(&boot, 0x30);
    let cpr = boot[0x40] as i8;
    let record_size = if cpr >= 0 {
        cpr as u64 * cluster_size
    } else {
        1u64 << (-cpr as u32)
    } as usize;
    if record_size == 0 || record_size > (1 << 20) {
        bail!("bad MFT record size {}", record_size);
    }
    Ok(Geometry {
        sector_size,
        cluster_size,
        record_size,
        mft_offset: mft_lcn * cluster_size,
    })
}

/// Apply the NTFS update-sequence-array fixup to a record buffer in place.
fn apply_fixup(rec: &mut [u8], sector_size: usize) -> bool {
    if rec.len() < 8 {
        return false;
    }
    let usa_off = ru16(rec, 0x04) as usize;
    let usa_cnt = ru16(rec, 0x06) as usize;
    if usa_cnt == 0 || usa_off + usa_cnt * 2 > rec.len() {
        return false;
    }
    for i in 1..usa_cnt {
        let sec_end = i * sector_size;
        if sec_end < 2 || sec_end > rec.len() {
            return false;
        }
        let save = usa_off + i * 2;
        rec[sec_end - 2] = rec[save];
        rec[sec_end - 1] = rec[save + 1];
    }
    true
}

/// Parse the $MFT's own $DATA runs (from record 0) into (lcn, cluster_count).
fn mft_data_runs(rec0: &[u8], _geo: &Geometry) -> Result<Vec<(u64, u64)>> {
    let first_attr = ru16(rec0, 0x14) as usize;
    let mut off = first_attr;
    while off + 8 <= rec0.len() {
        let atype = ru32(rec0, off);
        if atype == 0xFFFF_FFFF {
            break;
        }
        let alen = ru32(rec0, off + 4) as usize;
        if alen == 0 || off + alen > rec0.len() {
            break;
        }
        if atype == 0x80 {
            // $DATA — must be non-resident for the MFT.
            let non_res = rec0[off + 8];
            if non_res == 0 {
                bail!("$MFT $DATA is resident?!");
            }
            let runs_off = ru16(rec0, off + 0x20) as usize;
            return parse_runs(&rec0[off + runs_off..off + alen]);
        }
        off += alen;
    }
    bail!("no $DATA in $MFT record")
}

fn parse_runs(bytes: &[u8]) -> Result<Vec<(u64, u64)>> {
    let mut runs = Vec::new();
    let mut i = 0usize;
    let mut cur_lcn: i64 = 0;
    while i < bytes.len() {
        let head = bytes[i];
        if head == 0 {
            break;
        }
        i += 1;
        let len_bytes = (head & 0x0F) as usize;
        let off_bytes = (head >> 4) as usize;
        if len_bytes == 0 || i + len_bytes + off_bytes > bytes.len() {
            break;
        }
        let mut run_len: u64 = 0;
        for j in 0..len_bytes {
            run_len |= (bytes[i + j] as u64) << (8 * j);
        }
        i += len_bytes;
        if off_bytes == 0 {
            // sparse run: no LCN, skip (does not occur for $MFT).
            continue;
        }
        let mut run_off: i64 = 0;
        for j in 0..off_bytes {
            run_off |= (bytes[i + j] as i64) << (8 * j);
        }
        // sign-extend the signed LCN delta.
        if bytes[i + off_bytes - 1] & 0x80 != 0 {
            run_off |= -1i64 << (8 * off_bytes);
        }
        i += off_bytes;
        cur_lcn += run_off;
        if cur_lcn < 0 {
            break;
        }
        runs.push((cur_lcn as u64, run_len));
    }
    if runs.is_empty() {
        bail!("empty MFT run list");
    }
    Ok(runs)
}

/// Enumerate the MFT by reading it sequentially and decoding each record.
/// Returns the number of records emitted. Records 0..16 (NTFS metadata files)
/// are skipped. On any structural error, returns Err so the caller can fall
/// back to the USN-enumeration path.
pub fn enumerate_mft_raw<F: FnMut(RecordFull)>(vol: &Handle, mut cb: F) -> Result<u64> {
    let geo = read_geometry(vol)?;

    let mut rec0 = vec![0u8; geo.record_size];
    read_at(vol, geo.mft_offset, &mut rec0)?;
    if &rec0[0..4] != b"FILE" {
        bail!("MFT record 0 is not a FILE record");
    }
    apply_fixup(&mut rec0, geo.sector_size);
    let runs = mft_data_runs(&rec0, &geo)?;

    let block = 8 * 1024 * 1024usize;
    let block = block - (block % geo.record_size); // whole records per block
    let mut buf = vec![0u8; block];
    let mut record_number: u64 = 0;
    let mut emitted: u64 = 0;
    let mut name_buf: Vec<u16> = Vec::with_capacity(256);

    for (lcn, clusters) in runs {
        let run_bytes = clusters * geo.cluster_size;
        let base = lcn * geo.cluster_size;
        let mut done = 0u64;
        while done < run_bytes {
            let this = std::cmp::min(block as u64, run_bytes - done) as usize;
            read_at(vol, base + done, &mut buf[..this])?;
            let recs = this / geo.record_size;
            for r in 0..recs {
                let start = r * geo.record_size;
                let rec = &mut buf[start..start + geo.record_size];
                if record_number >= 16
                    && parse_file_record(rec, &geo, record_number, &mut name_buf, &mut cb)
                {
                    emitted += 1;
                }
                record_number += 1;
            }
            done += this as u64;
        }
    }
    Ok(emitted)
}

/// Decode one FILE record, invoking `cb` if it is an in-use base record with a
/// name. `name_buf` is a scratch buffer reused across calls.
fn parse_file_record<F: FnMut(RecordFull)>(
    rec: &mut [u8],
    geo: &Geometry,
    record_number: u64,
    name_buf: &mut Vec<u16>,
    cb: &mut F,
) -> bool {
    if rec.len() < 0x30 || &rec[0..4] != b"FILE" {
        return false;
    }
    if !apply_fixup(rec, geo.sector_size) {
        return false;
    }
    let flags = ru16(rec, 0x16);
    if flags & 0x01 == 0 {
        return false; // not in use
    }
    if ru64(rec, 0x20) != 0 {
        return false; // extension record (has a base); handled via its base
    }
    let is_dir = flags & 0x02 != 0;
    let seq = ru16(rec, 0x10) as u64;
    let frn = record_number | (seq << 48);
    let used = ru32(rec, 0x18) as usize;
    let limit = used.min(rec.len());

    let mut off = ru16(rec, 0x14) as usize;
    let mut best_rank = 255u8;
    let mut parent_frn = 0u64;
    let mut size = 0u64;
    let mut mtime = 0i64;
    let mut ctime = 0i64;
    let mut have_name = false;
    name_buf.clear();

    while off + 8 <= limit {
        let atype = ru32(rec, off);
        if atype == 0xFFFF_FFFF {
            break;
        }
        let alen = ru32(rec, off + 4) as usize;
        if alen < 0x18 || off + alen > rec.len() {
            break;
        }
        let non_res = rec[off + 8];
        let name_len_attr = rec[off + 9];

        match atype {
            0x10 => {
                // $STANDARD_INFORMATION (always resident)
                let voff = ru16(rec, off + 0x14) as usize;
                let b = off + voff;
                if b + 0x18 <= off + alen {
                    ctime = ri64(rec, b);
                    mtime = ri64(rec, b + 0x08);
                }
            }
            0x30 => {
                // $FILE_NAME (resident). Prefer Win32 names over DOS 8.3.
                let voff = ru16(rec, off + 0x14) as usize;
                let b = off + voff;
                if b + 0x42 <= off + alen {
                    let nlen = rec[b + 0x40] as usize;
                    let ns = rec[b + 0x41];
                    let rank = match ns {
                        3 => 0, // Win32 & DOS
                        1 => 1, // Win32
                        0 => 2, // POSIX
                        2 => 3, // DOS only
                        _ => 4,
                    };
                    let name_end = b + 0x42 + nlen * 2;
                    if rank < best_rank && name_end <= off + alen {
                        best_rank = rank;
                        parent_frn = ru64(rec, b);
                        name_buf.clear();
                        for k in 0..nlen {
                            name_buf.push(ru16(rec, b + 0x42 + k * 2));
                        }
                        have_name = true;
                    }
                }
            }
            0x80
                // $DATA — unnamed stream gives the file size.
                if name_len_attr == 0 => {
                    if non_res == 0 {
                        size = ru32(rec, off + 0x10) as u64;
                    } else if off + 0x38 <= rec.len() {
                        size = ru64(rec, off + 0x30); // real (logical) size
                    }
                }
            _ => {}
        }
        off += alen;
    }

    if !have_name {
        return false;
    }
    cb(RecordFull {
        frn,
        parent_frn,
        attributes: if is_dir { 0x10 } else { 0 },
        size: if is_dir { 0 } else { size },
        mtime,
        ctime,
        name: name_buf,
    });
    true
}

/// Decoded metadata for one file.
#[derive(Clone, Copy, Default)]
pub struct FileMeta {
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
}

fn filetime_to_i64(ft: windows_sys::Win32::Foundation::FILETIME) -> i64 {
    ((ft.dwHighDateTime as i64) << 32) | (ft.dwLowDateTime as i64 & 0xFFFF_FFFF)
}

/// Fetch size + timestamps for one file, identified by its FRN, using the
/// volume handle as an open-by-id hint.
pub fn fetch_meta(vol: &Handle, frn: u64) -> Option<FileMeta> {
    // FILE_ID_DESCRIPTOR with Type = FileIdType(0), FileId = frn.
    let mut desc: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
    desc.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
    desc.Type = 0; // FileIdType
                   // The union's first member is a 64-bit FileId (LARGE_INTEGER).
    unsafe {
        let p = &mut desc.Anonymous as *mut _ as *mut u64;
        *p = frn;
    }
    let h = unsafe {
        OpenFileById(
            vol.0,
            &desc,
            0, // no data access, attributes only
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            FILE_FLAG_BACKUP_SEMANTICS, // allow opening directories
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return None;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(h, &mut info) };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    Some(FileMeta {
        size,
        mtime: filetime_to_i64(info.ftLastWriteTime),
        ctime: filetime_to_i64(info.ftCreationTime),
    })
}
