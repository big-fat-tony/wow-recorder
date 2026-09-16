//! Chapter markers for finished MP4 files.
//!
//! OBS writes a plain MP4; after a run we add a Nero-style chapter list
//! (`moov/udta/chpl`) so players (VLC, mpv, MPC-HC, anything ffmpeg-based) show
//! the deaths as chapters. No re-mux: `moov` is rewritten in place when it is
//! the last box (OBS's layout), otherwise the old one is turned into `free`
//! and the new one appended — `mdat` never moves so chunk offsets stay valid.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    pub start_ms: u64,
    pub title: String,
}

/// `chpl` stores the chapter count in one byte.
const MAX_CHAPTERS: usize = 255;
/// ...and each title's length in one byte.
const MAX_TITLE_BYTES: usize = 255;

/// A top-level (or nested) box: where its header starts, how long the header
/// is, and the total size including the header.
#[derive(Debug, Clone, Copy)]
struct BoxInfo {
    offset: u64,
    header_len: u64,
    size: u64,
    kind: [u8; 4],
}

fn read_header<R: Read>(r: &mut R, offset: u64, end: u64) -> io::Result<Option<BoxInfo>> {
    if end - offset < 8 {
        return Ok(None);
    }
    let mut hdr = [0u8; 8];
    r.read_exact(&mut hdr)?;
    let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let kind = [hdr[4], hdr[5], hdr[6], hdr[7]];
    let (size, header_len) = match size32 {
        0 => (end - offset, 8), // extends to end of container
        1 => {
            let mut big = [0u8; 8];
            r.read_exact(&mut big)?;
            (u64::from_be_bytes(big), 16)
        }
        n => (n as u64, 8),
    };
    if size < header_len || offset + size > end {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad box size {size} at {offset}")));
    }
    Ok(Some(BoxInfo { offset, header_len, size, kind }))
}

/// Walk the boxes in an in-memory container payload.
fn boxes_in(buf: &[u8]) -> io::Result<Vec<BoxInfo>> {
    let mut out = Vec::new();
    let mut pos = 0u64;
    let end = buf.len() as u64;
    while pos < end {
        let mut cursor = &buf[pos as usize..];
        let Some(b) = read_header(&mut cursor, pos, end)? else { break };
        pos += b.size;
        out.push(b);
    }
    Ok(out)
}

fn make_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 16);
    let total = payload.len() as u64 + 8;
    if total <= u32::MAX as u64 {
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(kind);
    } else {
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(&(total + 8).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// Serialize a `chpl` box (version 1, as ffmpeg writes it).
fn chpl_box(chapters: &[Chapter]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&[1, 0, 0, 0]); // version 1, flags 0
    p.extend_from_slice(&[0, 0, 0, 0]); // reserved
    let chapters = &chapters[..chapters.len().min(MAX_CHAPTERS)];
    p.push(chapters.len() as u8);
    for c in chapters {
        p.extend_from_slice(&(c.start_ms * 10_000).to_be_bytes()); // 100 ns units
        let title = truncate_utf8(&c.title, MAX_TITLE_BYTES);
        p.push(title.len() as u8);
        p.extend_from_slice(title.as_bytes());
    }
    make_box(b"chpl", &p)
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Rebuild a `udta` payload with `chpl` replaced/appended.
fn udta_with_chapters(udta_payload: &[u8], chpl: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(udta_payload.len() + chpl.len());
    for b in boxes_in(udta_payload)? {
        if &b.kind != b"chpl" {
            out.extend_from_slice(&udta_payload[b.offset as usize..(b.offset + b.size) as usize]);
        }
    }
    out.extend_from_slice(chpl);
    Ok(out)
}

/// Rebuild a `moov` payload with the chapter list installed.
fn moov_with_chapters(moov_payload: &[u8], chapters: &[Chapter]) -> io::Result<Vec<u8>> {
    let chpl = chpl_box(chapters);
    let mut out = Vec::with_capacity(moov_payload.len() + chpl.len() + 8);
    let mut had_udta = false;
    for b in boxes_in(moov_payload)? {
        let raw = &moov_payload[b.offset as usize..(b.offset + b.size) as usize];
        if &b.kind == b"udta" {
            had_udta = true;
            let payload = &raw[b.header_len as usize..];
            out.extend_from_slice(&make_box(b"udta", &udta_with_chapters(payload, &chpl)?));
        } else {
            out.extend_from_slice(raw);
        }
    }
    if !had_udta {
        out.extend_from_slice(&make_box(b"udta", &chpl));
    }
    Ok(out)
}

/// Install `chapters` into the MP4 at `path`, replacing any existing `chpl`.
pub fn write_chapters(path: &Path, chapters: &[Chapter]) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();

    // Locate the top-level `moov`.
    let mut pos = 0u64;
    let mut moov = None;
    while pos < len {
        file.seek(SeekFrom::Start(pos))?;
        let Some(b) = read_header(&mut file, pos, len)? else { break };
        if &b.kind == b"moov" {
            moov = Some(b);
            break;
        }
        pos += b.size;
    }
    let moov = moov.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no moov box (unfinalized mp4?)"))?;

    file.seek(SeekFrom::Start(moov.offset + moov.header_len))?;
    let mut payload = vec![0u8; (moov.size - moov.header_len) as usize];
    file.read_exact(&mut payload)?;
    let new_moov = make_box(b"moov", &moov_with_chapters(&payload, chapters)?);

    if moov.offset + moov.size == len {
        // Last box: rewrite in place.
        file.set_len(moov.offset)?;
        file.seek(SeekFrom::Start(moov.offset))?;
    } else {
        // Something (mdat) follows: retire the old moov as `free`, append the new one.
        file.seek(SeekFrom::Start(moov.offset + 4))?;
        file.write_all(b"free")?;
        file.seek(SeekFrom::End(0))?;
    }
    file.write_all(&new_moov)?;
    file.flush()
}

/// Read the `chpl` chapter list back out of an MP4 (used by tests).
#[cfg(test)]
pub fn read_chapters(path: &Path) -> io::Result<Vec<Chapter>> {
    let data = std::fs::read(path)?;
    let moov = boxes_in(&data)?
        .into_iter()
        .find(|b| &b.kind == b"moov")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no moov"))?;
    let moov_payload = &data[(moov.offset + moov.header_len) as usize..(moov.offset + moov.size) as usize];
    let Some(udta) = boxes_in(moov_payload)?.into_iter().find(|b| &b.kind == b"udta") else {
        return Ok(vec![]);
    };
    let udta_payload = &moov_payload[(udta.offset + udta.header_len) as usize..(udta.offset + udta.size) as usize];
    let Some(chpl) = boxes_in(udta_payload)?.into_iter().find(|b| &b.kind == b"chpl") else {
        return Ok(vec![]);
    };
    let p = &udta_payload[(chpl.offset + chpl.header_len) as usize..(chpl.offset + chpl.size) as usize];
    let count = p[8] as usize;
    let mut out = Vec::with_capacity(count);
    let mut i = 9;
    for _ in 0..count {
        let start = u64::from_be_bytes(p[i..i + 8].try_into().unwrap());
        let n = p[i + 8] as usize;
        let title = String::from_utf8_lossy(&p[i + 9..i + 9 + n]).into_owned();
        out.push(Chapter { start_ms: start / 10_000, title });
        i += 9 + n;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("wow-recorder-chapters-{}-{name}.mp4", std::process::id()))
    }

    fn ftyp() -> Vec<u8> {
        make_box(b"ftyp", b"isom\0\0\x02\0isomiso2mp41")
    }

    fn mvhd() -> Vec<u8> {
        make_box(b"mvhd", &[0u8; 100])
    }

    fn chapters() -> Vec<Chapter> {
        vec![
            Chapter { start_ms: 0, title: "Start".into() },
            Chapter { start_ms: 859_500, title: "Death: Tonyfat".into() },
        ]
    }

    #[test]
    fn moov_at_end_is_rewritten_in_place() {
        let path = tmp("end");
        let mdat = make_box(b"mdat", &[7u8; 64]);
        let mut file = ftyp();
        file.extend_from_slice(&mdat);
        file.extend_from_slice(&make_box(b"moov", &mvhd()));
        std::fs::write(&path, &file).unwrap();

        write_chapters(&path, &chapters()).unwrap();
        assert_eq!(read_chapters(&path).unwrap(), chapters());
        // mdat is untouched and still directly after ftyp; moov is still last and only one exists.
        let data = std::fs::read(&path).unwrap();
        let kinds: Vec<_> = boxes_in(&data).unwrap().into_iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![*b"ftyp", *b"mdat", *b"moov"]);
        assert_eq!(&data[ftyp().len()..ftyp().len() + mdat.len()], &mdat[..]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn moov_before_mdat_is_freed_and_appended() {
        let path = tmp("front");
        let mut file = ftyp();
        let moov_offset = file.len();
        file.extend_from_slice(&make_box(b"moov", &mvhd()));
        let mdat_offset = file.len();
        let mdat = make_box(b"mdat", &[9u8; 32]);
        file.extend_from_slice(&mdat);
        std::fs::write(&path, &file).unwrap();

        write_chapters(&path, &chapters()).unwrap();
        assert_eq!(read_chapters(&path).unwrap(), chapters());
        let data = std::fs::read(&path).unwrap();
        let kinds: Vec<_> = boxes_in(&data).unwrap().into_iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![*b"ftyp", *b"free", *b"mdat", *b"moov"]);
        assert_eq!(&data[moov_offset + 4..moov_offset + 8], b"free");
        assert_eq!(&data[mdat_offset..mdat_offset + mdat.len()], &mdat[..]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn existing_udta_and_chpl_are_replaced() {
        let path = tmp("replace");
        let old = chpl_box(&[Chapter { start_ms: 1, title: "old".into() }]);
        let mut udta = make_box(b"meta", &[0u8; 12]);
        udta.extend_from_slice(&old);
        let mut moov = mvhd();
        moov.extend_from_slice(&make_box(b"udta", &udta));
        let mut file = ftyp();
        file.extend_from_slice(&make_box(b"moov", &moov));
        std::fs::write(&path, &file).unwrap();

        write_chapters(&path, &chapters()).unwrap();
        assert_eq!(read_chapters(&path).unwrap(), chapters());
        // The unrelated `meta` child survived and there is exactly one chpl.
        let data = std::fs::read(&path).unwrap();
        let count = data.windows(4).filter(|w| w == b"chpl").count();
        assert_eq!(count, 1);
        assert!(data.windows(4).any(|w| w == b"meta"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn long_titles_are_truncated_on_char_boundaries() {
        let title = "Ã¸".repeat(200); // 400 bytes
        let ch = vec![Chapter { start_ms: 5, title: title.clone() }];
        let bytes = chpl_box(&ch);
        let len = bytes[8 + 9 + 8] as usize;
        assert_eq!(len, 254);
        assert!(std::str::from_utf8(&bytes[8 + 9 + 9..8 + 9 + 9 + len]).is_ok());
    }

    #[test]
    fn no_moov_is_an_error() {
        let path = tmp("nomoov");
        std::fs::write(&path, ftyp()).unwrap();
        assert!(write_chapters(&path, &chapters()).is_err());
        std::fs::remove_file(&path).ok();
    }
}
