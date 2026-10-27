//! Payloads and chunked bodies for both directions, with the framing
//! defects either side may send (FR-PROXY-19), and where each defect
//! starts.

use super::*;

/// A chunked framing defect, on either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkDefect {
    Extension,
    Trailer,
    SignedSize,
    SpaceSize,
    Overflow,
    DataTooLong,
    BareLf,
    LongSize,
    EmptySize,
}

impl ChunkDefect {
    pub(crate) fn error(self) -> ChunkedError {
        match self {
            ChunkDefect::Extension => ChunkedError::Extension,
            ChunkDefect::Trailer => ChunkedError::Trailer,
            ChunkDefect::SignedSize
            | ChunkDefect::SpaceSize
            | ChunkDefect::LongSize
            | ChunkDefect::EmptySize => ChunkedError::BadSize,
            ChunkDefect::Overflow => ChunkedError::SizeOverflow,
            ChunkDefect::DataTooLong | ChunkDefect::BareLf => ChunkedError::BadLineEnd,
        }
    }

    pub(crate) fn tag(self) -> &'static str {
        match self {
            ChunkDefect::Extension => "chunk extension",
            ChunkDefect::Trailer => "chunk trailer",
            ChunkDefect::SignedSize | ChunkDefect::SpaceSize => "bad chunk size",
            ChunkDefect::LongSize => "chunk size over 32 characters",
            ChunkDefect::EmptySize => "empty chunk size",
            ChunkDefect::Overflow => "chunk size overflow",
            ChunkDefect::DataTooLong | ChunkDefect::BareLf => "bad chunk line end",
        }
    }

    pub(crate) fn random(rng: &mut fastrand::Rng) -> ChunkDefect {
        match rng.u8(0..9) {
            7 => ChunkDefect::LongSize,
            8 => ChunkDefect::EmptySize,
            0 => ChunkDefect::Extension,
            1 => ChunkDefect::Trailer,
            2 => ChunkDefect::SignedSize,
            3 => ChunkDefect::SpaceSize,
            4 => ChunkDefect::Overflow,
            5 => ChunkDefect::DataTooLong,
            _ => ChunkDefect::BareLf,
        }
    }
}

/// A chunked body: its wire bytes, and for every payload byte its offset
/// in the wire; with a defect, the offset of the first byte that makes the
/// framing invalid.
pub(crate) struct Chunked {
    pub(crate) wire: Vec<u8>,
    pub(crate) payload_at: Vec<usize>,
    pub(crate) defect_at: Option<usize>,
}

fn hex(rng: &mut fastrand::Rng, n: usize) -> String {
    let zeros = "0".repeat(if rng.u8(0..4) == 0 {
        rng.usize(1..4)
    } else {
        0
    });
    if rng.bool() {
        format!("{zeros}{n:x}")
    } else {
        format!("{zeros}{n:X}")
    }
}

/// Encode `payload` as random chunks, with `defect` in a random place.
pub(crate) fn chunked(
    rng: &mut fastrand::Rng,
    payload: &[u8],
    defect: Option<ChunkDefect>,
) -> Chunked {
    let mut sizes = Vec::new();
    let mut left = payload.len();
    while left > 0 {
        let n = match rng.u8(0..4) {
            0 => left,
            1 => rng.usize(1..=left.min(8)),
            _ => rng.usize(1..=left),
        };
        sizes.push(n);
        left -= n;
    }
    // The defect sits in chunk `at` (== sizes.len(): the last chunk).
    let at = rng.usize(0..=sizes.len());
    let mut out = Chunked {
        wire: Vec::new(),
        payload_at: Vec::new(),
        defect_at: None,
    };
    let mut offset = 0;
    for (i, &n) in sizes.iter().enumerate() {
        let here = defect.filter(|_| i == at);
        let size = hex(rng, n);
        let start = out.wire.len();
        match here {
            Some(ChunkDefect::SignedSize) => {
                out.defect_at = Some(start);
                out.wire
                    .extend_from_slice(format!("+{size}\r\n").as_bytes());
            }
            Some(ChunkDefect::SpaceSize) => {
                out.defect_at = Some(start);
                out.wire
                    .extend_from_slice(format!(" {size}\r\n").as_bytes());
            }
            // The 17th significant digit is the first that cannot fit.
            Some(ChunkDefect::Overflow) => {
                out.defect_at = Some(start + 16);
                out.wire
                    .extend_from_slice(format!("1{:016x}\r\n", n).as_bytes());
            }
            // 33 characters: the 33rd is one too many.
            Some(ChunkDefect::LongSize) => {
                out.defect_at = Some(start + 32);
                out.wire
                    .extend_from_slice(format!("{size:0>33}\r\n").as_bytes());
            }
            Some(ChunkDefect::EmptySize) => {
                out.defect_at = Some(start);
                out.wire.extend_from_slice(b"\r\n");
            }
            Some(ChunkDefect::Extension) => {
                out.defect_at = Some(start + size.len());
                out.wire
                    .extend_from_slice(format!("{size};ext=1\r\n").as_bytes());
            }
            Some(ChunkDefect::BareLf) => {
                out.defect_at = Some(start + size.len());
                out.wire.extend_from_slice(format!("{size}\n").as_bytes());
            }
            Some(ChunkDefect::Trailer | ChunkDefect::DataTooLong) | None => {
                out.wire.extend_from_slice(format!("{size}\r\n").as_bytes());
            }
        }
        for b in &payload[offset..offset + n] {
            out.payload_at.push(out.wire.len());
            out.wire.push(*b);
        }
        offset += n;
        if here == Some(ChunkDefect::DataTooLong) {
            out.defect_at = Some(out.wire.len());
            out.wire.push(b'x');
        }
        out.wire.extend_from_slice(b"\r\n");
        if out.defect_at.is_some() {
            return out;
        }
    }
    match defect {
        Some(ChunkDefect::Trailer) => {
            out.wire.extend_from_slice(b"0\r\n");
            out.defect_at = Some(out.wire.len());
            out.wire.extend_from_slice(b"X-Trailer: 1\r\n\r\n");
        }
        // The defect in the last chunk's size line (`0`).
        Some(ChunkDefect::SignedSize) => {
            out.defect_at = Some(out.wire.len());
            out.wire.extend_from_slice(b"+0\r\n\r\n");
        }
        Some(ChunkDefect::SpaceSize) => {
            out.defect_at = Some(out.wire.len());
            out.wire.extend_from_slice(b" 0\r\n\r\n");
        }
        Some(ChunkDefect::Overflow) => {
            out.defect_at = Some(out.wire.len() + 16);
            out.wire.extend_from_slice(b"10000000000000000\r\n\r\n");
        }
        Some(ChunkDefect::LongSize) => {
            out.defect_at = Some(out.wire.len() + 32);
            out.wire
                .extend_from_slice(format!("{:0>33}\r\n\r\n", 0).as_bytes());
        }
        Some(ChunkDefect::EmptySize) => {
            out.defect_at = Some(out.wire.len());
            out.wire.extend_from_slice(b"\r\n\r\n");
        }
        Some(ChunkDefect::Extension) => {
            out.defect_at = Some(out.wire.len() + 1);
            out.wire.extend_from_slice(b"0;x\r\n\r\n");
        }
        Some(ChunkDefect::BareLf) => {
            out.defect_at = Some(out.wire.len() + 1);
            out.wire.extend_from_slice(b"0\n\r\n");
        }
        // A one-byte chunk with two data bytes.
        Some(ChunkDefect::DataTooLong) => {
            out.wire.extend_from_slice(b"1\r\n");
            out.payload_at.push(out.wire.len());
            out.wire.push(b'x');
            out.defect_at = Some(out.wire.len());
            out.wire.extend_from_slice(b"y\r\n0\r\n\r\n");
        }
        None => out.wire.extend_from_slice(b"0\r\n\r\n"),
    }
    out
}

/// Random payload bytes; some carry a whole request or CRLFs, which must
/// travel as plain body bytes.
pub(crate) fn payload(rng: &mut fastrand::Rng, len: usize) -> Vec<u8> {
    const HIDDEN: &[u8] = b"\r\n\r\nGET /smuggled HTTP/1.1\r\nHost: example.test\r\n\r\n";
    let mut out = Vec::with_capacity(len);
    let mode = rng.u8(0..4);
    while out.len() < len {
        match mode {
            0 => out.extend_from_slice(HIDDEN),
            1 => out.push(rng.u8(..)),
            _ => out.push(b"abcdefgh0123\r\n"[out.len() % 14]),
        }
    }
    out.truncate(len);
    out
}

/// The offsets in `c.wire` where a unit of chunked framing ends: after
/// each data byte, and after each complete line (size line, the CRLF
/// after data), the last chunk counting as one (`0` CRLF CRLF). A body
/// forwarded so far ends at one of them: an incomplete size line or CRLF
/// is held back, never data (NFR-RES-04).
pub(crate) fn unit_ends(c: &Chunked) -> Vec<usize> {
    let mut data = vec![false; c.wire.len()];
    for &at in &c.payload_at {
        data[at] = true;
    }
    let mut ends = Vec::new();
    for i in 0..c.wire.len() {
        let line_end =
            c.wire[i] == b'\n' && !data[i] && i > 0 && c.wire[i - 1] == b'\r' && !data[i - 1];
        if data[i] || line_end {
            ends.push(i + 1);
        }
    }
    // The last chunk (`0` CRLF CRLF) is held back as a whole.
    if c.defect_at.is_none() {
        let last_size_line = c.wire.len().saturating_sub(2);
        ends.retain(|&e| e != last_size_line);
    }
    ends
}

/// How much of a chunked body the core may have forwarded once the first
/// `j` bytes of it arrived: up to the last unit end at or before `j`.
pub(crate) fn forwardable(ends: &[usize], j: usize) -> usize {
    ends.iter()
        .copied()
        .take_while(|&e| e <= j)
        .last()
        .unwrap_or(0)
}
