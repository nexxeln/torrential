use std::time::Duration;

const LONGEST: Duration = Duration::from_secs(48 * 3600);

/// Box types that open an MP4 or QuickTime file.
const FIRST_BOXES: [&[u8]; 7] = [b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"pnot"];

const EBML: u64 = 0x1A45_DFA3;
const SEGMENT: u64 = 0x1853_8067;
const INFO: u64 = 0x1549_A966;
const TIMECODE_SCALE: u64 = 0x2A_D7B1;
const DURATION: u64 = 0x4489;

/// The playing time recorded in an MP4/MOV or Matroska/WebM header, read from the first and last bytes of the file.
pub fn duration(head: &[u8], tail: &[u8]) -> Option<Duration> {
    let runtime = matroska(head).or_else(|| mp4(head, tail))?;
    (runtime > Duration::ZERO && runtime <= LONGEST).then_some(runtime)
}

fn mp4(head: &[u8], tail: &[u8]) -> Option<Duration> {
    if !FIRST_BOXES.contains(&head.get(4..8)?) {
        return None;
    }
    movie_header(head).or_else(|| movie_header(tail))
}

/// The first `mvhd` box anywhere in `bytes`. The tail starts mid-file, so there is no box boundary to walk from.
fn movie_header(bytes: &[u8]) -> Option<Duration> {
    (0..bytes.len()).filter_map(|at| bytes.get(at..)).find_map(mvhd)
}

fn mvhd(bytes: &[u8]) -> Option<Duration> {
    if bytes.get(4..8)? != b"mvhd" {
        return None;
    }
    let size = uint(bytes.get(..4)?)?;
    let (timescale, duration) = match *bytes.get(8)? {
        0 if size >= 108 => (bytes.get(20..24)?, bytes.get(24..28)?),
        1 if size >= 120 => (bytes.get(28..32)?, bytes.get(32..40)?),
        _ => return None,
    };
    if duration.iter().all(|&b| b == 0xff) {
        return None;
    }
    let nanos = (u128::from(uint(duration)?) * 1_000_000_000).checked_div(u128::from(uint(timescale)?))?;
    Some(Duration::from_nanos(u64::try_from(nanos).ok()?))
}

fn matroska(head: &[u8]) -> Option<Duration> {
    let (EBML, size, rest) = header(head)? else {
        return None;
    };
    // The Segment's size is ignored because it runs past the head, or is the unknown-size marker in live files.
    let (SEGMENT, _, segment) = header(rest.get(usize::try_from(size).ok()?..)?)? else {
        return None;
    };
    let (_, info) = elements(segment).find(|&(id, _)| id == INFO)?;
    let mut scale = 1_000_000;
    let mut length = None;
    for (id, body) in elements(info) {
        match id {
            TIMECODE_SCALE => scale = uint(body)?,
            DURATION => length = Some(float(body)?),
            _ => {}
        }
    }
    Duration::try_from_secs_f64(length? * scale as f64 / 1e9).ok()
}

/// The whole elements at the start of `bytes`, as ID and body, up to the first one that is cut off.
fn elements(mut bytes: &[u8]) -> impl Iterator<Item = (u64, &[u8])> {
    std::iter::from_fn(move || {
        let (id, size, rest) = header(bytes)?;
        let size = usize::try_from(size).ok()?;
        let body = rest.get(..size)?;
        bytes = rest.get(size..)?;
        Some((id, body))
    })
}

/// An element's ID, its body size, and the bytes after the header.
fn header(bytes: &[u8]) -> Option<(u64, u64, &[u8])> {
    let id_width = width(*bytes.first()?).filter(|&n| n <= 4)?;
    let id = uint(bytes.get(..id_width)?)?;
    let rest = bytes.get(id_width..)?;
    let size_width = width(*rest.first()?)?;
    let mask = (1u64 << (7 * size_width)) - 1;
    Some((id, uint(rest.get(..size_width)?)? & mask, rest.get(size_width..)?))
}

/// The byte length of an EBML variable-length integer, from its first byte.
fn width(first: u8) -> Option<usize> {
    (first != 0).then(|| first.leading_zeros() as usize + 1)
}

fn uint(bytes: &[u8]) -> Option<u64> {
    (bytes.len() <= 8).then(|| bytes.iter().fold(0, |n, &b| n << 8 | u64::from(b)))
}

fn float(bytes: &[u8]) -> Option<f64> {
    match *bytes {
        [a, b, c, d] => Some(f32::from_be_bytes([a, b, c, d]).into()),
        [a, b, c, d, e, f, g, h] => Some(f64::from_be_bytes([a, b, c, d, e, f, g, h])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEVEN: Duration = Duration::from_secs(7);
    const NINETY_MINUTES: Duration = Duration::from_secs(90 * 60);

    fn clip(name: &str) -> Vec<u8> {
        let path = format!("{}/tests/fixtures/media/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn ends(file: &[u8], n: usize) -> Option<Duration> {
        duration(&file[..n], &file[file.len() - n..])
    }

    fn mp4_file(version: u8, timescale: u32, duration: u64) -> Vec<u8> {
        let mut mvhd = [0u8; 120];
        mvhd[4..8].copy_from_slice(b"mvhd");
        mvhd[8] = version;
        let fields = if version == 0 {
            mvhd[3] = 108;
            mvhd[20..24].copy_from_slice(&timescale.to_be_bytes());
            mvhd[24..28].copy_from_slice(&(duration as u32).to_be_bytes());
            &mvhd[..108]
        } else {
            mvhd[3] = 120;
            mvhd[28..32].copy_from_slice(&timescale.to_be_bytes());
            mvhd[32..40].copy_from_slice(&duration.to_be_bytes());
            &mvhd[..]
        };
        let mut file = b"\0\0\0\x10ftypisom\0\0\x02\0".to_vec();
        file.extend((8 + fields.len() as u32).to_be_bytes());
        file.extend(b"moov");
        file.extend(fields);
        file
    }

    fn mkv_file(info: &[u8]) -> Vec<u8> {
        let mut file = vec![0x1a, 0x45, 0xdf, 0xa3, 0x80, 0x18, 0x53, 0x80, 0x67, 0xff, 0x15, 0x49, 0xa9, 0x66];
        file.push(0x80 | info.len() as u8);
        file.extend(info);
        file
    }

    #[test]
    fn reads_runtime_from_container_headers() {
        let (faststart, moov_at_end, mkv) = (clip("faststart.mp4"), clip("moov-at-end.mp4"), clip("clip.mkv"));
        let garbage: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let avi = b"RIFF\x00\x10\x00\x00AVI LIST\x00\x01\x00\x00hdrlavih".to_vec();
        let ts = [0x47, 0x40, 0x00, 0x10].repeat(512);
        let read = [
            ends(&faststart, 1024),
            ends(&moov_at_end, 1024),
            duration(&moov_at_end[..1024], &[]),
            ends(&mkv, 1024),
            ends(&garbage, 1024),
            ends(&avi, 24),
            ends(&ts, 1024),
            duration(&[], &[]),
        ];
        assert_eq!(read, [Some(SEVEN), Some(SEVEN), None, Some(SEVEN), None, None, None, None]);
    }

    #[test]
    fn reads_a_segment_of_unknown_size() {
        let mut mkv = clip("clip.mkv");
        assert_eq!(mkv[0x28..0x2c], [0x18, 0x53, 0x80, 0x67]);
        mkv.splice(0x2c..0x34, [0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(ends(&mkv, 1024), Some(SEVEN));
    }

    #[test]
    fn reads_both_header_versions_and_float_widths() {
        let read = [
            duration(&mp4_file(0, 600, 3_240_000), &[]),
            duration(&mp4_file(1, 90_000, 486_000_000), &[]),
            duration(
                &mkv_file(&[0x2a, 0xd7, 0xb1, 0x84, 0x3b, 0x9a, 0xca, 0x00, 0x44, 0x89, 0x84, 0x45, 0xa8, 0xc0, 0x00]),
                &[],
            ),
            duration(&mkv_file(&[0x44, 0x89, 0x88, 0x41, 0x54, 0x99, 0x70, 0x00, 0x00, 0x00, 0x00]), &[]),
        ];
        assert_eq!(read, [Some(NINETY_MINUTES); 4]);
    }

    #[test]
    fn rejects_absurd_runtimes() {
        let read = [
            duration(&mp4_file(1, 0, 486_000_000), &[]),
            duration(&mp4_file(1, 1000, 0), &[]),
            duration(&mp4_file(1, 1000, 49 * 3600 * 1000), &[]),
            duration(&mp4_file(0, 90_000, u64::from(u32::MAX)), &[]),
            duration(&mp4_file(1, 90_000, u64::MAX), &[]),
            duration(&mkv_file(&[0x44, 0x89, 0x84, 0xbf, 0x80, 0x00, 0x00]), &[]),
            duration(&mkv_file(&[0x44, 0x89, 0x84, 0x7f, 0xc0, 0x00, 0x00]), &[]),
            duration(&mp4_file(1, 1000, 48 * 3600 * 1000), &[]),
        ];
        assert_eq!(read, [None, None, None, None, None, None, None, Some(Duration::from_secs(48 * 3600))]);
    }

    #[test]
    fn cut_headers_read_only_once_the_fields_are_whole() {
        let shortest_head = |file: &[u8]| (0..=file.len()).find(|&n| duration(&file[..n], &[]).is_some());
        let shortest_tail = |file: &[u8]| {
            let head = &file[..1024];
            (0..=file.len()).find(|&n| duration(head, &file[file.len() - n..]).is_some())
        };
        let (faststart, moov_at_end, mkv) = (clip("faststart.mp4"), clip("moov-at-end.mp4"), clip("clip.mkv"));
        let scale_last =
            mkv_file(&[0x44, 0x89, 0x84, 0x45, 0xa8, 0xc0, 0x00, 0x2a, 0xd7, 0xb1, 0x84, 0x3b, 0x9a, 0xca, 0x00]);
        assert_eq!(
            [shortest_head(&faststart), shortest_tail(&moov_at_end), shortest_head(&mkv), shortest_head(&scale_last)],
            [Some(68), Some(941), Some(291), Some(30)]
        );
    }
}
