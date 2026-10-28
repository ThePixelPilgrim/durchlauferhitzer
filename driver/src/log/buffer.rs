//! The sink's bounded buffer of whole entries (FR-LOG-06), without I/O.
//!
//! Bytes go out in order from the front. A new entry that does not fit
//! is dropped whole and counted; one that went out in part keeps its
//! remainder, so a line is never cut. The count is queued as its own
//! `kind=dropped count=N` entry as soon as it fits, before any newer
//! entry (R4).

use std::collections::VecDeque;

use super::DriverLog;
use super::format::format_driver;

/// The sink's buffer size (FR-LOG-06).
pub(crate) const CAPACITY: usize = 64 * 1024;

/// What a buffered entry stands for, so a hard write error can count
/// what it loses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stands {
    /// One entry.
    Entry,
    /// A `dropped` line: this many entries.
    Dropped(u64),
}

/// One buffered entry: its bytes not written yet (the front one may have
/// gone out in part).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Queued {
    left: usize,
    stands: Stands,
}

#[derive(Debug)]
pub(crate) struct EntryBuffer {
    capacity: usize,
    bytes: VecDeque<u8>,
    entries: VecDeque<Queued>,
    /// Entries dropped and not yet reported.
    dropped: u64,
    /// Scratch space for the `dropped` line.
    line: Vec<u8>,
}

impl EntryBuffer {
    pub(crate) fn new(capacity: usize) -> EntryBuffer {
        EntryBuffer {
            capacity,
            bytes: VecDeque::with_capacity(capacity),
            entries: VecDeque::new(),
            dropped: 0,
            line: Vec::new(),
        }
    }

    /// Queues one whole line, after the `dropped` line if one is due and
    /// fits; a line that does not fit is dropped and counted.
    pub(crate) fn push(&mut self, line: &[u8]) {
        self.catch_up();
        if !self.queue(line, Stands::Entry) {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    /// Queues the `dropped` line if entries were dropped and it fits.
    pub(crate) fn catch_up(&mut self) {
        if self.dropped == 0 {
            return;
        }
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        format_driver(
            &DriverLog::Dropped {
                count: self.dropped,
            },
            &mut line,
        );
        if self.queue(&line, Stands::Dropped(self.dropped)) {
            self.dropped = 0;
        }
        self.line = line;
    }

    /// Appends `line` if it fits; whether it did.
    fn queue(&mut self, line: &[u8], stands: Stands) -> bool {
        if line.len() > self.capacity.saturating_sub(self.bytes.len()) {
            return false;
        }
        self.bytes.extend(line);
        self.entries.push_back(Queued {
            left: line.len(),
            stands,
        });
        true
    }

    /// The next bytes to write (empty when nothing is buffered).
    pub(crate) fn front(&self) -> &[u8] {
        self.bytes.as_slices().0
    }

    /// `n` bytes from the front were written.
    pub(crate) fn consume(&mut self, n: usize) {
        let n = n.min(self.bytes.len());
        self.bytes.drain(..n);
        let mut n = n;
        while n > 0 {
            let Some(front) = self.entries.front_mut() else {
                return;
            };
            if front.left > n {
                front.left -= n;
                return;
            }
            n -= front.left;
            self.entries.pop_front();
        }
    }

    /// A hard write error: everything buffered is lost and counted (an
    /// entry written in part counts as dropped too).
    pub(crate) fn discard(&mut self) {
        for queued in self.entries.drain(..) {
            let lost = match queued.stands {
                Stands::Entry => 1,
                Stands::Dropped(count) => count,
            };
            self.dropped = self.dropped.saturating_add(lost);
        }
        self.bytes.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::EntryBuffer;

    /// Everything the buffer would write, consumed at once.
    fn take_all(buffer: &mut EntryBuffer) -> String {
        let mut out = Vec::new();
        while !buffer.is_empty() {
            let front = buffer.front().to_vec();
            out.extend_from_slice(&front);
            buffer.consume(front.len());
            buffer.catch_up();
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn entries_come_out_in_order() {
        let mut buffer = EntryBuffer::new(64);
        buffer.push(b"a=1\n");
        buffer.push(b"b=2\n");
        assert_eq!(take_all(&mut buffer), "a=1\nb=2\n");
    }

    /// R4: an entry written in part keeps its remainder; it is never
    /// dropped to make room, so the line is never cut.
    #[test]
    fn a_partly_written_entry_keeps_its_remainder() {
        let mut buffer = EntryBuffer::new(24);
        buffer.push(b"abcdef\n");
        buffer.consume(3);
        // No room for 21 more bytes: the new entry is the one dropped.
        buffer.push(b"ghijklmnopqrstuvwxyz\n");
        assert_eq!(take_all(&mut buffer), "def\nkind=dropped count=1\n");
    }

    /// FR-LOG-06: a full buffer drops whole new entries and counts them;
    /// the count is written once there is room, before newer entries.
    #[test]
    fn a_full_buffer_drops_and_counts_then_reports_once() {
        let mut buffer = EntryBuffer::new(30);
        buffer.push(b"012345678901234567890123456\n");
        buffer.push(b"x=1\n");
        buffer.push(b"x=2\n");
        // Room again: the next entry comes after the count.
        buffer.consume(28);
        buffer.push(b"x=3\n");
        assert_eq!(take_all(&mut buffer), "kind=dropped count=2\nx=3\n");
        buffer.push(b"x=4\n");
        assert_eq!(take_all(&mut buffer), "x=4\n");
    }

    /// The count waits while even its own line does not fit.
    #[test]
    fn the_count_waits_for_room_for_its_line() {
        let mut buffer = EntryBuffer::new(24);
        buffer.push(b"01234567890123456789012\n");
        buffer.push(b"x=1\n");
        buffer.consume(4);
        buffer.catch_up();
        buffer.consume(20);
        assert!(buffer.is_empty());
        buffer.catch_up();
        assert_eq!(take_all(&mut buffer), "kind=dropped count=1\n");
    }

    /// An entry larger than the whole buffer can never be written: it is
    /// dropped and counted like any other.
    #[test]
    fn an_entry_larger_than_the_buffer_is_dropped() {
        let mut buffer = EntryBuffer::new(4);
        buffer.push(b"toolong\n");
        assert!(buffer.is_empty());
        let mut buffer = EntryBuffer::new(32);
        buffer.push(&[b'x'; 33]);
        buffer.catch_up();
        assert_eq!(take_all(&mut buffer), "kind=dropped count=1\n");
    }

    /// A hard write error loses what is buffered: each entry counts as
    /// dropped, a queued count line counts as the entries it stood for.
    #[test]
    fn discarding_counts_every_entry_it_loses() {
        let mut buffer = EntryBuffer::new(30);
        buffer.push(b"012345678901234567890123456\n");
        buffer.push(b"x=1\n");
        buffer.consume(28);
        buffer.push(b"x=2\n");
        // Buffered now: the count line (1) and x=2.
        buffer.consume(5);
        buffer.discard();
        assert!(buffer.is_empty());
        buffer.catch_up();
        assert_eq!(take_all(&mut buffer), "kind=dropped count=2\n");
    }
}
