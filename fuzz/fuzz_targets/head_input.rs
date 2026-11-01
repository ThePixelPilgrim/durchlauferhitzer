//! How the head targets read a fuzz input: an 8-byte prefix chooses the
//! limits, a cut point and the request kind; the rest is the head.

use durchlauferhitzer_core::http::request::HeadLimits;

pub struct HeadInput<'a> {
    /// Limits the config could set: each at least 1,
    /// `max_request_line <= max_head` (FR-CFG).
    pub limits: HeadLimits,
    /// The bytes before the cut are fed first (0..=bytes.len()).
    pub cut: usize,
    /// Bit 0 picks the request kind of the response-head target.
    pub kind: u8,
    pub bytes: &'a [u8],
}

impl<'a> HeadInput<'a> {
    pub fn read(data: &'a [u8]) -> Option<Self> {
        let [h0, h1, l0, l1, n, c0, c1, kind, bytes @ ..] = data else {
            return None;
        };
        let max_head = 1 + usize::from(u16::from_le_bytes([*h0, *h1])) % 4096;
        let max_request_line = 1 + usize::from(u16::from_le_bytes([*l0, *l1])) % max_head;
        let max_headers = 1 + usize::from(*n) % 128;
        let cut = usize::from(u16::from_le_bytes([*c0, *c1])) % (bytes.len() + 1);
        Some(Self {
            limits: HeadLimits {
                max_request_line,
                max_head,
                max_headers,
            },
            cut,
            kind: *kind,
            bytes,
        })
    }
}
