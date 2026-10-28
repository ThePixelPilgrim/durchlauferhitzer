//! Upstream (backend) sockets: the non-blocking connect and its answer,
//! reads and writes by interest, and the rules that keep a backend's
//! readable bytes ahead of its socket error (DRV-10).

use std::io;
use std::os::fd::AsFd;

use durchlauferhitzer_core::{BackendId, ConnError, Event, Interest, UpstreamId};

use super::{Broken, Driver, conn_error, token};
use crate::conns::{Answer, Eof, Phase, Token, Upstream, Writes, has_read, has_write};
use crate::log::{DriverLog, LogEntry};
use crate::sys::epoll::{Readiness, Trouble};
use crate::sys::fd::Io;
use crate::sys::socket::{ConnectStart, connect_nonblocking, read, set_nodelay, take_error, write};

/// Whether a failed write is reported at once or left to the reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteMode {
    /// Interest `Write`, or `ReadWrite` after the end of stream: a failed
    /// write is reported at once.
    Alone,
    /// Interest `ReadWrite` before the end of stream: the driver stops
    /// writing and goes on reading; the reads report the error after the
    /// readable bytes.
    WithReads,
}

/// How an open upstream writes now: `None` when it does not write at all.
/// Under `ReadWrite` a failed write is left to the reads, unless the end
/// of stream was already delivered: no read follows then, so the write
/// reports its error at once.
pub(super) fn write_mode(interest: Interest, writes: Writes, eof: Eof) -> Option<WriteMode> {
    match (interest, writes, eof) {
        (Interest::Write, _, _) | (Interest::ReadWrite, Writes::Open, Eof::Delivered) => {
            Some(WriteMode::Alone)
        }
        (Interest::ReadWrite, Writes::Open, Eof::NotYet) => Some(WriteMode::WithReads),
        (Interest::ReadWrite, Writes::Failed, _) | (Interest::None | Interest::Read, _, _) => None,
    }
}

impl Driver {
    /// `Connect`: start the non-blocking connect now; its outcome is always
    /// delivered as a later event (DRV-10), also when `connect()` or
    /// `socket()` decides at once.
    pub(super) fn connect(&mut self, up: UpstreamId, backend: BackendId) -> io::Result<()> {
        let addr = usize::try_from(backend.0)
            .ok()
            .and_then(|i| self.core.backends().get(i))
            .copied();
        let phase = match addr.map(connect_nonblocking) {
            // A backend id outside `backends()` would be a core bug; it is
            // a failed connect like any other.
            None => Phase::Answering(Answer::Failed),
            Some(Err(err)) => {
                // `socket()` failed (DRV-10): a 502; descriptor exhaustion
                // is logged distinctly (FR-LOG-07).
                if let Some(errno @ (libc::EMFILE | libc::ENFILE)) = err.raw_os_error() {
                    self.log
                        .write(LogEntry::Driver(DriverLog::SocketFailed { errno }));
                }
                Phase::Answering(Answer::Failed)
            }
            Some(Ok(connecting)) => {
                // NFR-PERF-04; set before the connect completes, never
                // fails on a TCP socket that exists.
                let _ = set_nodelay(connecting.fd.as_fd());
                match connecting.immediate {
                    Ok(ConnectStart::InProgress) => Phase::Connecting(connecting.fd),
                    Ok(ConnectStart::Connected) => {
                        Phase::Answering(Answer::Connected(connecting.fd))
                    }
                    Err(_) => Phase::Answering(Answer::Failed),
                }
            }
        };
        let answering = matches!(phase, Phase::Answering(_));
        self.upstreams.insert(
            up,
            Upstream {
                phase,
                interest: Interest::None,
                registered: None,
            },
        );
        if answering {
            self.answers.push_back(up);
        }
        self.sync_upstream(up)
    }

    /// The queued answer of `up`'s connect, if `CloseUpstream` did not come
    /// first (then nothing is delivered for it).
    pub(super) fn take_answer(&mut self, up: UpstreamId) -> io::Result<Option<Event>> {
        let Some(upstream) = self.upstreams.get_mut(&up) else {
            return Ok(None);
        };
        let phase = std::mem::replace(&mut upstream.phase, Phase::Failed);
        let event = match phase {
            Phase::Answering(Answer::Connected(fd)) => {
                upstream.phase = Phase::Open {
                    fd,
                    writes: Writes::Open,
                    eof: Eof::NotYet,
                };
                Event::UpstreamConnected { up }
            }
            Phase::Answering(Answer::Failed) => Event::UpstreamFailed {
                up,
                error: ConnError::Other,
            },
            other @ (Phase::Connecting(_) | Phase::Open { .. } | Phase::Failed) => {
                upstream.phase = other;
                return Ok(None);
            }
        };
        self.sync_upstream(up)?;
        Ok(Some(event))
    }

    /// Readiness on an upstream socket.
    pub(super) fn upstream_event(
        &mut self,
        up: UpstreamId,
        readiness: Readiness,
        trouble: Trouble,
    ) -> io::Result<()> {
        let Some(upstream) = self.upstreams.get(&up) else {
            return Ok(());
        };
        let interest = upstream.interest;
        match &upstream.phase {
            Phase::Connecting(_) => return self.finish_connect(up),
            // Out of the epoll set: a stale event from earlier in the
            // batch.
            Phase::Answering(_) | Phase::Failed => return Ok(()),
            Phase::Open { .. } => {}
        }
        // An error or hang-up is learnt by reading or writing, never
        // reported by itself (DRV-10).
        let troubled = match trouble {
            Trouble::None => false,
            Trouble::Hangup | Trouble::Error | Trouble::ErrorHangup => true,
        };
        match interest {
            // Deferred: a stale event from before the socket left the
            // epoll set; the error waits until the core reads or writes.
            Interest::None => Ok(()),
            Interest::Read => {
                if has_read(readiness) || troubled {
                    self.upstream_read(up)?;
                }
                Ok(())
            }
            Interest::Write => {
                if has_write(readiness) || troubled {
                    self.upstream_write(up)?;
                }
                Ok(())
            }
            Interest::ReadWrite => {
                // Reads first, so the backend's bytes come before any
                // error a write would hit.
                if has_read(readiness) || troubled {
                    self.upstream_read(up)?;
                }
                if has_write(readiness) {
                    self.upstream_write(up)?;
                }
                Ok(())
            }
        }
    }

    /// The pending connect became writable (or failed): `SO_ERROR` decides
    /// (DRV-10).
    fn finish_connect(&mut self, up: UpstreamId) -> io::Result<()> {
        let Some(upstream) = self.upstreams.get_mut(&up) else {
            return Ok(());
        };
        let phase = std::mem::replace(&mut upstream.phase, Phase::Failed);
        let Phase::Connecting(fd) = phase else {
            upstream.phase = phase;
            return Ok(());
        };
        let event = match take_error(fd.as_fd()) {
            Ok(None) => {
                upstream.phase = Phase::Open {
                    fd,
                    writes: Writes::Open,
                    eof: Eof::NotYet,
                };
                Event::UpstreamConnected { up }
            }
            // Every failed connect is `Other` (the contract). Closing the
            // socket takes it out of the epoll set.
            Ok(Some(_)) | Err(_) => {
                drop(fd);
                Event::UpstreamFailed {
                    up,
                    error: ConnError::Other,
                }
            }
        };
        self.sync_upstream(up)?;
        self.deliver(event)
    }

    /// Reads into `upstream_recv_buffer` until it is empty or the socket
    /// would block, each read reported as the very next call. The end of
    /// stream and the socket error come only from a read, after every
    /// readable byte (DRV-10).
    fn upstream_read(&mut self, up: UpstreamId) -> io::Result<()> {
        loop {
            let Some(upstream) = self.upstreams.get(&up) else {
                return Ok(());
            };
            let Phase::Open { fd, eof, .. } = &upstream.phase else {
                return Ok(());
            };
            let reading = match upstream.interest {
                Interest::Read | Interest::ReadWrite => *eof == Eof::NotYet,
                Interest::None | Interest::Write => false,
            };
            if !reading {
                return Ok(());
            }
            let buf = self.core.upstream_recv_buffer(up);
            if buf.is_empty() {
                return Ok(());
            }
            let event = match read(fd.as_fd(), buf) {
                Io::Done(0) | Io::WouldBlock => return Ok(()),
                Io::Done(len) => Event::UpstreamReceived { up, len },
                Io::Closed => {
                    self.upstream_eof(up)?;
                    Event::UpstreamReadClosed { up }
                }
                Io::Err(err) => self.upstream_failed(up, conn_error(&err))?,
            };
            let more = matches!(event, Event::UpstreamReceived { .. });
            self.deliver(event)?;
            if !more {
                return Ok(());
            }
        }
    }

    /// Writes from `upstream_send_buffer` until it is empty or the socket
    /// would block, each write reported as the very next call.
    fn upstream_write(&mut self, up: UpstreamId) -> io::Result<()> {
        loop {
            let Some(upstream) = self.upstreams.get(&up) else {
                return Ok(());
            };
            let Phase::Open { fd, writes, eof } = &upstream.phase else {
                return Ok(());
            };
            let Some(mode) = write_mode(upstream.interest, *writes, *eof) else {
                return Ok(());
            };
            let buf = self.core.upstream_send_buffer(up);
            if buf.is_empty() {
                return Ok(());
            }
            let error = match write(fd.as_fd(), buf) {
                Io::Done(0) | Io::WouldBlock => return Ok(()),
                Io::Done(len) => {
                    self.deliver(Event::UpstreamSent { up, len })?;
                    continue;
                }
                // `send` of a non-empty buffer never returns 0.
                Io::Closed => ConnError::Other,
                Io::Err(err) => conn_error(&err),
            };
            return match mode {
                WriteMode::Alone => {
                    let event = self.upstream_failed(up, error)?;
                    self.deliver(event)
                }
                WriteMode::WithReads => self.stop_writing(up),
            };
        }
    }

    /// A write failed under `ReadWrite`: no more writes; the socket stays
    /// registered for reading, whose results report the error.
    fn stop_writing(&mut self, up: UpstreamId) -> io::Result<()> {
        if let Some(Upstream {
            phase: Phase::Open { writes, .. },
            ..
        }) = self.upstreams.get_mut(&up)
        {
            *writes = Writes::Failed;
        }
        self.sync_upstream(up)
    }

    /// The end of stream was read: reported once, not read again.
    fn upstream_eof(&mut self, up: UpstreamId) -> io::Result<()> {
        if let Some(Upstream {
            phase: Phase::Open { eof, .. },
            ..
        }) = self.upstreams.get_mut(&up)
        {
            *eof = Eof::Delivered;
        }
        self.sync_upstream(up)
    }

    /// The socket failed: it is closed now (the core does no more with it
    /// than `CloseUpstream`), and the event to report is returned.
    fn upstream_failed(&mut self, up: UpstreamId, error: ConnError) -> io::Result<Event> {
        if let Some(upstream) = self.upstreams.get_mut(&up) {
            upstream.phase = Phase::Failed;
        }
        self.sync_upstream(up)?;
        Ok(Event::UpstreamFailed { up, error })
    }

    pub(super) fn set_upstream_interest(
        &mut self,
        up: UpstreamId,
        interest: Interest,
    ) -> io::Result<()> {
        if let Some(upstream) = self.upstreams.get_mut(&up) {
            upstream.interest = interest;
        }
        self.sync_upstream(up)
    }

    /// Brings the upstream's epoll registration in line with what it
    /// wants: added, modified or removed. A failed `epoll_ctl` fails only
    /// this upstream (DRV-15): it is logged and the socket closed; a
    /// pending connect is answered `UpstreamFailed` like any failed
    /// connect, an open upstream is reported `UpstreamFailed` after the
    /// actions of the current call.
    pub(super) fn sync_upstream(&mut self, up: UpstreamId) -> io::Result<()> {
        let Some(upstream) = self.upstreams.get_mut(&up) else {
            return Ok(());
        };
        let wanted = upstream.wanted();
        if wanted == upstream.registered {
            return Ok(());
        }
        let fd = match &upstream.phase {
            Phase::Connecting(fd) | Phase::Open { fd, .. } => fd.as_fd(),
            Phase::Answering(Answer::Connected(fd)) => fd.as_fd(),
            // The socket is closed, which took it out of the set.
            Phase::Answering(Answer::Failed) | Phase::Failed => {
                upstream.registered = None;
                return Ok(());
            }
        };
        let raw = token(Token::Upstream(up))?;
        let result = match (upstream.registered, wanted) {
            (None, Some(readiness)) => self.epoll.add(fd, raw, readiness),
            (Some(_), Some(readiness)) => self.epoll.modify(fd, raw, readiness),
            (Some(_), None) => self.epoll.delete(fd),
            (None, None) => Ok(()),
        };
        let Err(err) = result else {
            upstream.registered = wanted;
            return Ok(());
        };
        let errno = err.raw_os_error().unwrap_or(0);
        self.log
            .write(LogEntry::Driver(DriverLog::RegisterFailed { errno }));
        // Closing the socket takes it out of the epoll set.
        upstream.registered = None;
        match upstream.phase {
            Phase::Connecting(_) => {
                upstream.phase = Phase::Answering(Answer::Failed);
                self.answers.push_back(up);
            }
            Phase::Open { .. } => {
                upstream.phase = Phase::Failed;
                self.broken
                    .push_back(Broken::Upstream(up, conn_error(&err)));
            }
            // Never registered, so never here.
            Phase::Answering(_) | Phase::Failed => {}
        }
        Ok(())
    }

    /// Closes the socket, also while its connect is pending; nothing more
    /// is delivered for the id (a queued answer included).
    pub(super) fn close_upstream(&mut self, up: UpstreamId) {
        self.upstreams.remove(&up);
    }
}
