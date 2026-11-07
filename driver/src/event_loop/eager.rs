//! Eager I/O and deferred registrations (NFR-PERF-05, owner decision
//! 2026-11-07): a proxied body used to cost about four `epoll_ctl` per
//! 16 KiB chunk, as each chunk flipped both sockets' interest (the
//! upstream out of the set and back, the client to `Write` and back).
//!
//! Now an interest the core sets is only noted. Before the loop waits
//! again, [`Driver::settle`] first tries each noted socket at once where
//! its hint says the I/O is likely to go through (a read that filled the
//! whole buffer, a write that emptied it, or epoll's last report): the
//! same reads and writes, by the same interest rules, as a readiness
//! event would bring (DRV-03 allows a driver that does not wait for
//! readiness; `EAGAIN` is never reported). Then it brings every noted
//! socket's registration in line with what it wants then. A chunk that
//! goes straight through leaves both registrations as they were: no
//! `epoll_ctl` at all. A socket that blocks is registered as before.
//!
//! Until then a registration may be stale. That is harmless: a readiness
//! the interest does not ask for is ignored (as for any stale event,
//! DRV-05), and every registration is in line again before the wait, so
//! nothing is waited for that the interest does not want (no spin on a
//! level-triggered descriptor, and an upstream with interest `None` is
//! out of the set, DRV-10).

use std::io;

use durchlauferhitzer_core::{ConnId, Interest, UpstreamId};

use super::Driver;
use crate::conns::{Eof, Health, Hint, Phase, Writes};

/// A client or upstream socket whose interest the core set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Socket {
    Client(ConnId),
    Upstream(UpstreamId),
}

/// The most eager attempts in one [`Driver::settle`], so a fast relay
/// cannot hold the loop from its other descriptors and timers (as
/// [`super::clients::ACCEPT_BATCH`] bounds accepts): 64 attempts move
/// half a MiB of 16 KiB chunks; what is left waits for its readiness.
pub(super) const EAGER_BUDGET: usize = 64;

/// Which I/O an eager attempt does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    Nothing,
    Read,
    Write,
    ReadWrite,
}

impl Driver {
    /// The core set `socket`'s interest: tried and synced in
    /// [`Driver::settle`].
    pub(super) fn note(&mut self, socket: Socket) {
        self.noted.push_back(socket);
    }

    /// Eager I/O for the noted sockets, within [`EAGER_BUDGET`], then
    /// every noted socket's registration brought in line; until nothing
    /// is noted, or the loop is ending (DRV-14: nothing more is done).
    pub(super) fn settle(&mut self) -> io::Result<()> {
        let mut budget = EAGER_BUDGET;
        loop {
            if self.ending().is_some() {
                return Ok(());
            }
            if let Some(socket) = self.noted.pop_front() {
                self.unsynced.push(socket);
                if budget > 0 && self.eager(socket)? {
                    budget -= 1;
                }
                self.flush_queued()?;
                continue;
            }
            let Some(socket) = self.unsynced.pop() else {
                return Ok(());
            };
            match socket {
                Socket::Client(conn) => self.sync_client(conn)?,
                Socket::Upstream(up) => self.sync_upstream(up)?,
            }
            // A failed registration queues the socket's failure (DRV-15).
            self.flush_queued()?;
        }
    }

    /// The I/O `socket`'s interest allows and its hints expect to go
    /// through, done now; whether any was tried.
    fn eager(&mut self, socket: Socket) -> io::Result<bool> {
        match socket {
            Socket::Client(conn) => {
                let attempt = match self.clients.get(&conn) {
                    Some(c) if c.health == Health::Ok => attempt(
                        c.interest,
                        c.eof == Eof::NotYet && c.hints.read == Hint::Ready,
                        c.hints.write == Hint::Ready,
                    ),
                    Some(_) | None => Attempt::Nothing,
                };
                match attempt {
                    Attempt::Nothing => return Ok(false),
                    Attempt::Read => self.client_read(conn)?,
                    // `ReadWrite` is never set for a client.
                    Attempt::Write | Attempt::ReadWrite => self.client_write(conn)?,
                }
            }
            Socket::Upstream(up) => {
                let attempt = match self.upstreams.get(&up) {
                    Some(u) => match &u.phase {
                        Phase::Open { writes, eof, .. } => attempt(
                            u.interest,
                            *eof == Eof::NotYet && u.hints.read == Hint::Ready,
                            *writes == Writes::Open && u.hints.write == Hint::Ready,
                        ),
                        Phase::Connecting(_) | Phase::Answering(_) | Phase::Failed => {
                            Attempt::Nothing
                        }
                    },
                    None => Attempt::Nothing,
                };
                match attempt {
                    Attempt::Nothing => return Ok(false),
                    Attempt::Read => self.upstream_read(up)?,
                    Attempt::Write => self.upstream_write(up)?,
                    // Reads first, as for a readiness event.
                    Attempt::ReadWrite => {
                        self.upstream_read(up)?;
                        self.upstream_write(up)?;
                    }
                }
            }
        }
        Ok(true)
    }
}

/// What to try for `interest` when reading and writing are each likely
/// to go through (`read`, `write`).
fn attempt(interest: Interest, read: bool, write: bool) -> Attempt {
    match (interest, read, write) {
        (Interest::Read | Interest::ReadWrite, true, false) => Attempt::Read,
        (Interest::Write | Interest::ReadWrite, false, true) => Attempt::Write,
        (Interest::ReadWrite, true, true) => Attempt::ReadWrite,
        (Interest::Read, true, true) => Attempt::Read,
        (Interest::Write, true, true) => Attempt::Write,
        (Interest::None, _, _)
        | (_, false, false)
        | (Interest::Read, false, true)
        | (Interest::Write, true, false) => Attempt::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attempt_needs_the_interest_and_the_hint() {
        use Attempt as A;
        use Interest as I;
        let cases = [
            (I::None, true, true, A::Nothing),
            (I::Read, true, false, A::Read),
            (I::Read, true, true, A::Read),
            (I::Read, false, true, A::Nothing),
            (I::Write, false, true, A::Write),
            (I::Write, true, true, A::Write),
            (I::Write, true, false, A::Nothing),
            (I::ReadWrite, true, true, A::ReadWrite),
            (I::ReadWrite, true, false, A::Read),
            (I::ReadWrite, false, true, A::Write),
            (I::ReadWrite, false, false, A::Nothing),
        ];
        for (interest, read, write, expected) in cases {
            assert_eq!(
                attempt(interest, read, write),
                expected,
                "{interest:?} {read} {write}"
            );
        }
    }
}
