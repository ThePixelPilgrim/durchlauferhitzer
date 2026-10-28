//! The loop's tables: what each epoll token names, and the state the
//! driver keeps per client and per upstream socket (the core keeps
//! everything else).
//!
//! A token is a `u64`: the kind in the low [`TAG_BITS`] bits, the id above
//! them. Connection ids are allocated by the driver from 1 and never
//! reused (DRV-01), upstream ids by the core, sequentially from 1, so a
//! token names one socket for the whole run: an event that arrives for a
//! socket closed earlier in the same `epoll_wait` batch finds no entry and
//! is dropped (DRV-05).

use std::os::fd::OwnedFd;

use durchlauferhitzer_core::{ConnError, ConnId, Interest, UpstreamId};

use crate::sys::epoll::Readiness;

/// What an epoll registration belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token {
    /// The listener at this index of the driver's listener list.
    Listener(u32),
    Client(ConnId),
    Upstream(UpstreamId),
    /// The eventfd that wakes the loop (the stop handle and the disk
    /// workers).
    Wake,
    /// The signalfd (Task 9).
    Signal,
    /// The log sink's descriptor (stderr), registered for writability
    /// only while the sink holds bytes (FR-LOG-06).
    Log,
}

/// Bits of a token that hold its kind.
const TAG_BITS: u32 = 3;
/// The largest id a token can carry (2^61 - 1).
const MAX_ID: u64 = u64::MAX >> TAG_BITS;

const LISTENER: u64 = 0;
const CLIENT: u64 = 1;
const UPSTREAM: u64 = 2;
const WAKE: u64 = 3;
const SIGNAL: u64 = 4;
const LOG: u64 = 5;

impl Token {
    /// The token as epoll stores it; `None` for an id above [`MAX_ID`]
    /// (2^61 connections or upstreams: never reached in practice).
    pub(crate) fn encode(self) -> Option<u64> {
        let (tag, id) = match self {
            Token::Listener(i) => (LISTENER, u64::from(i)),
            Token::Client(ConnId(id)) => (CLIENT, id),
            Token::Upstream(UpstreamId(id)) => (UPSTREAM, id),
            Token::Wake => (WAKE, 0),
            Token::Signal => (SIGNAL, 0),
            Token::Log => (LOG, 0),
        };
        (id <= MAX_ID).then_some((id << TAG_BITS) | tag)
    }

    /// The token `encode` made `raw` from; `None` for a value no token
    /// encodes to.
    pub(crate) fn decode(raw: u64) -> Option<Token> {
        let id = raw >> TAG_BITS;
        match raw & ((1 << TAG_BITS) - 1) {
            LISTENER => u32::try_from(id).ok().map(Token::Listener),
            CLIENT => Some(Token::Client(ConnId(id))),
            UPSTREAM => Some(Token::Upstream(UpstreamId(id))),
            WAKE if id == 0 => Some(Token::Wake),
            SIGNAL if id == 0 => Some(Token::Signal),
            LOG if id == 0 => Some(Token::Log),
            _ => None,
        }
    }
}

/// Whether the end of stream was read (and reported) on a socket. It is
/// reported once; afterwards the socket is not read again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Eof {
    NotYet,
    Delivered,
}

/// Whether a client's epoll registration holds. After an `epoll_ctl`
/// failure the client is reported `Failed` and the driver does no more
/// I/O on it (DRV-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    Ok,
    Broken,
}

/// A client connection the core knows (from its `Accepted` until its
/// `Close`).
#[derive(Debug)]
pub(crate) struct Client {
    pub(crate) fd: OwnedFd,
    /// What the core last asked for; `None` until its first `SetInterest`.
    pub(crate) interest: Interest,
    pub(crate) eof: Eof,
    pub(crate) health: Health,
    /// What the socket is registered for. A client stays in the epoll
    /// set, so its errors and hang-ups are seen whatever the interest
    /// (DRV-10), until it is broken (DRV-15).
    pub(crate) registered: Readiness,
}

impl Client {
    pub(crate) fn new(fd: OwnedFd) -> Client {
        Client {
            fd,
            interest: Interest::None,
            eof: Eof::NotYet,
            health: Health::Ok,
            registered: Readiness::None,
        }
    }

    /// The readiness the socket should be registered for now.
    pub(crate) fn wanted(&self) -> Readiness {
        let read = match self.eof {
            Eof::NotYet => Readiness::Read,
            Eof::Delivered => Readiness::None,
        };
        match self.interest {
            Interest::None => Readiness::None,
            Interest::Read => read,
            Interest::Write => Readiness::Write,
            // Never set for a client; waiting for both is harmless.
            Interest::ReadWrite => combine(read, Readiness::Write),
        }
    }
}

/// Whether an upstream's writes still go through. After a write fails
/// under `ReadWrite`, the driver stops writing and goes on reading: the
/// reads report the error after the readable bytes (DRV-10), or, once
/// they reach the end of stream, the error kept here right after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Writes {
    Open,
    Failed(ConnError),
}

/// Where an upstream's connect stands.
#[derive(Debug)]
pub(crate) enum Phase {
    /// `connect` is in progress: the driver waits for `EPOLLOUT` itself
    /// (whatever the interest) and then reads `SO_ERROR`.
    Connecting(OwnedFd),
    /// The connect's outcome is known and waits to be delivered as a later
    /// event (DRV-10): it succeeded or failed at once.
    Answering(Answer),
    /// Connected (`UpstreamConnected` delivered).
    Open {
        fd: OwnedFd,
        writes: Writes,
        eof: Eof,
    },
    /// `UpstreamFailed` was delivered: the socket is closed and nothing
    /// more is delivered; the entry waits for `CloseUpstream`.
    Failed,
}

/// A connect outcome that is delivered later.
#[derive(Debug)]
pub(crate) enum Answer {
    Connected(OwnedFd),
    Failed,
}

/// An upstream the core opened (from its `Connect` until its
/// `CloseUpstream`).
#[derive(Debug)]
pub(crate) struct Upstream {
    pub(crate) phase: Phase,
    /// What the core last asked for; `None` until its first
    /// `SetUpstreamInterest`.
    pub(crate) interest: Interest,
    /// What the socket is registered for; `None` (the Rust `Option`) while
    /// it is out of the epoll set.
    pub(crate) registered: Option<Readiness>,
}

impl Upstream {
    /// The registration the socket should have now: `EPOLLOUT` while it
    /// connects; once open, the interest, but out of the epoll set while
    /// that is `None`, so an error waits until the core asks to read or
    /// write again (DRV-10: epoll reports `EPOLLERR`/`EPOLLHUP` whatever
    /// was asked for).
    pub(crate) fn wanted(&self) -> Option<Readiness> {
        match &self.phase {
            Phase::Connecting(_) => Some(Readiness::Write),
            Phase::Answering(_) | Phase::Failed => None,
            Phase::Open { writes, eof, .. } => {
                let read = match eof {
                    Eof::NotYet => Readiness::Read,
                    Eof::Delivered => Readiness::None,
                };
                let write = match writes {
                    Writes::Open => Readiness::Write,
                    Writes::Failed(_) => Readiness::None,
                };
                let readiness = match self.interest {
                    Interest::None => Readiness::None,
                    Interest::Read => read,
                    // A failed write under `Write` alone is reported at
                    // once, so the next write attempt must happen.
                    Interest::Write => Readiness::Write,
                    Interest::ReadWrite => combine(read, write),
                };
                match readiness {
                    Readiness::None => None,
                    Readiness::Read | Readiness::Write | Readiness::ReadWrite => Some(readiness),
                }
            }
        }
    }
}

/// Whether a readiness includes reading.
pub(crate) fn has_read(readiness: Readiness) -> bool {
    match readiness {
        Readiness::Read | Readiness::ReadWrite => true,
        Readiness::None | Readiness::Write => false,
    }
}

/// Whether a readiness includes writing.
pub(crate) fn has_write(readiness: Readiness) -> bool {
    match readiness {
        Readiness::Write | Readiness::ReadWrite => true,
        Readiness::None | Readiness::Read => false,
    }
}

fn combine(a: Readiness, b: Readiness) -> Readiness {
    match (has_read(a) || has_read(b), has_write(a) || has_write(b)) {
        (false, false) => Readiness::None,
        (true, false) => Readiness::Read,
        (false, true) => Readiness::Write,
        (true, true) => Readiness::ReadWrite,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fd() -> OwnedFd {
        OwnedFd::from(std::fs::File::open("/dev/null").unwrap())
    }

    fn open(interest: Interest, writes: Writes, eof: Eof) -> Upstream {
        Upstream {
            phase: Phase::Open {
                fd: fd(),
                writes,
                eof,
            },
            interest,
            registered: None,
        }
    }

    #[test]
    fn a_connecting_upstream_waits_for_writability_whatever_the_interest() {
        for interest in [
            Interest::None,
            Interest::Read,
            Interest::Write,
            Interest::ReadWrite,
        ] {
            let up = Upstream {
                phase: Phase::Connecting(fd()),
                interest,
                registered: None,
            };
            assert_eq!(up.wanted(), Some(Readiness::Write), "{interest:?}");
        }
    }

    #[test]
    fn an_upstream_without_interest_leaves_the_epoll_set() {
        // DRV-10: its errors wait until the core reads or writes again.
        assert_eq!(
            open(Interest::None, Writes::Open, Eof::NotYet).wanted(),
            None
        );
        let answering = Upstream {
            phase: Phase::Answering(Answer::Failed),
            interest: Interest::Read,
            registered: None,
        };
        assert_eq!(answering.wanted(), None);
    }

    #[test]
    fn an_open_upstream_waits_for_its_interest() {
        let cases = [
            (
                Interest::Read,
                Writes::Open,
                Eof::NotYet,
                Some(Readiness::Read),
            ),
            (
                Interest::Write,
                Writes::Open,
                Eof::NotYet,
                Some(Readiness::Write),
            ),
            (
                Interest::ReadWrite,
                Writes::Open,
                Eof::NotYet,
                Some(Readiness::ReadWrite),
            ),
            // After a failed write under `ReadWrite`, only the reads go on.
            (
                Interest::ReadWrite,
                Writes::Failed(ConnError::Reset),
                Eof::NotYet,
                Some(Readiness::Read),
            ),
            // Under `Write` alone the failure is reported by the next write.
            (
                Interest::Write,
                Writes::Failed(ConnError::Reset),
                Eof::NotYet,
                Some(Readiness::Write),
            ),
            // The end of stream is read once.
            (Interest::Read, Writes::Open, Eof::Delivered, None),
            (
                Interest::ReadWrite,
                Writes::Open,
                Eof::Delivered,
                Some(Readiness::Write),
            ),
        ];
        for (interest, writes, eof, wanted) in cases {
            let up = open(interest, writes, eof);
            assert_eq!(up.wanted(), wanted, "{interest:?} {writes:?} {eof:?}");
        }
    }

    #[test]
    fn a_client_waits_for_its_interest_and_always_stays_registered() {
        let cases = [
            (Interest::None, Eof::NotYet, Readiness::None),
            (Interest::Read, Eof::NotYet, Readiness::Read),
            (Interest::Write, Eof::NotYet, Readiness::Write),
            (Interest::Read, Eof::Delivered, Readiness::None),
        ];
        for (interest, eof, wanted) in cases {
            let mut client = Client::new(fd());
            client.interest = interest;
            client.eof = eof;
            assert_eq!(client.wanted(), wanted, "{interest:?} {eof:?}");
        }
    }

    #[test]
    fn tokens_round_trip() {
        let tokens = [
            Token::Listener(0),
            Token::Listener(u32::MAX),
            Token::Client(ConnId(1)),
            Token::Client(ConnId(MAX_ID)),
            Token::Upstream(UpstreamId(1)),
            Token::Upstream(UpstreamId(MAX_ID)),
            Token::Wake,
            Token::Signal,
            Token::Log,
        ];
        for token in tokens {
            let raw = token.encode().unwrap();
            assert_eq!(Token::decode(raw), Some(token), "{raw:#x}");
        }
    }

    #[test]
    fn kinds_never_share_a_token() {
        let raws = [
            Token::Listener(5).encode(),
            Token::Client(ConnId(5)).encode(),
            Token::Upstream(UpstreamId(5)).encode(),
        ];
        assert_ne!(raws[0], raws[1]);
        assert_ne!(raws[1], raws[2]);
        assert_ne!(raws[0], raws[2]);
    }

    #[test]
    fn ids_beyond_the_token_space_do_not_encode() {
        assert_eq!(Token::Client(ConnId(MAX_ID + 1)).encode(), None);
        assert_eq!(Token::Upstream(UpstreamId(u64::MAX)).encode(), None);
    }

    #[test]
    fn values_no_token_encodes_to_are_rejected() {
        for tag in 5..8u64 {
            assert_eq!(Token::decode((7 << TAG_BITS) | tag), None);
        }
        assert_eq!(Token::decode((1 << TAG_BITS) | WAKE), None);
        assert_eq!(Token::decode((1 << TAG_BITS) | SIGNAL), None);
        assert_eq!(Token::decode((u64::from(u32::MAX) + 1) << TAG_BITS), None);
    }
}
