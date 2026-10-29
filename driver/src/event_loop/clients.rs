//! Client connections: accepting them, reading into the core's receive
//! buffer, writing from its send buffer, and the client actions.

use std::io;
use std::os::fd::AsFd;

use durchlauferhitzer_core::{ConnError, ConnId, Event, Interest};

use super::accept::{AcceptError, Accepting, Logging, Resume, classify};
use super::{Broken, Driver, Freed, conn_error, log_register_failed, token};
use crate::conns::{Client, Eof, Health, Token};
use crate::log::{DriverLog, LogEntry};
use crate::sys::epoll::{Readiness, Trouble};
use crate::sys::fd::Io;
use crate::sys::socket::{
    accept_nonblocking, read, set_nodelay, shutdown_write, take_error, write,
};

/// The most connections one readiness of a listener accepts (review A,
/// Minor 3): a flood of connects cannot hold the loop from its other
/// descriptors and timers. The rest wait in the backlog; the listener is
/// level-triggered, so the next wait reports it again at once.
pub(super) const ACCEPT_BATCH: usize = 64;

impl Driver {
    /// Accepts the pending connections on listener `i`, until it would
    /// block or [`ACCEPT_BATCH`] are in. A failure that is not transient is
    /// logged (at most once a second) and pauses every listener (DRV-16).
    pub(super) fn accept_all(&mut self, i: u32) -> io::Result<()> {
        for _ in 0..ACCEPT_BATCH {
            match self.accepting.state() {
                Accepting::Open => {}
                Accepting::Paused { .. } => return Ok(()),
            }
            let Some(listener) = usize::try_from(i).ok().and_then(|i| self.listeners.get(i)) else {
                return Ok(());
            };
            match accept_nonblocking(listener.as_fd()) {
                Ok(Some((fd, peer))) => self.accepted(fd, peer.ip())?,
                Ok(None) => return Ok(()),
                Err(err) => match classify(&err) {
                    AcceptError::Transient => {}
                    AcceptError::Pause(errno) => {
                        match self.accepting.failed(self.clock.now()) {
                            Logging::Log => self
                                .log
                                .write(LogEntry::Driver(DriverLog::AcceptFailed { errno })),
                            Logging::Quiet => {}
                        }
                        return self.register_listeners(Readiness::None);
                    }
                },
            }
        }
        Ok(())
    }

    /// Paused listeners resume when a step closed a descriptor (DRV-16).
    pub(super) fn freed(&mut self, freed: Freed) -> io::Result<()> {
        match freed {
            Freed::Descriptor => self.descriptor_closed(),
            Freed::Nothing => Ok(()),
        }
    }

    /// The driver closed a descriptor: paused listeners resume at once
    /// (DRV-16).
    pub(super) fn descriptor_closed(&mut self) -> io::Result<()> {
        match self.accepting.descriptor_closed() {
            Resume::Resumed => self.register_listeners(Readiness::Read),
            Resume::Unchanged => Ok(()),
        }
    }

    /// Paused listeners whose retry instant has passed resume (DRV-16).
    pub(super) fn retry_accept(&mut self) -> io::Result<()> {
        match self.accepting.tick(self.clock.now()) {
            Resume::Resumed => self.register_listeners(Readiness::Read),
            Resume::Unchanged => Ok(()),
        }
    }

    /// Puts the listeners into the wait (`Read`) or takes them out
    /// (`None`). A failure here is the loop's own and ends `run`.
    fn register_listeners(&mut self, readiness: Readiness) -> io::Result<()> {
        for (i, listener) in self.listeners.iter().enumerate() {
            let Ok(i) = u32::try_from(i) else {
                continue;
            };
            self.epoll
                .modify(listener.as_fd(), token(Token::Listener(i))?, readiness)?;
        }
        Ok(())
    }

    /// A new connection: `TCP_NODELAY` (NFR-PERF-04; buffer sizes are left
    /// to the kernel), a fresh id (DRV-01), into the epoll set, then
    /// `Accepted`.
    pub(super) fn accepted(
        &mut self,
        fd: std::os::fd::OwnedFd,
        peer: std::net::IpAddr,
    ) -> io::Result<()> {
        // Cannot fail on a freshly accepted TCP socket; a socket that
        // somehow refuses it still works.
        let _ = set_nodelay(fd.as_fd());
        let conn = ConnId(self.next_conn);
        // 2^61 connections are never reached; the socket is dropped then.
        let Some(raw) = Token::Client(conn).encode() else {
            return Ok(());
        };
        if let Err(err) = self.epoll.add(fd.as_fd(), raw, Readiness::None) {
            // DRV-15: the core never heard of it (its first event must be
            // `Accepted`), so the socket is dropped and the reason logged.
            log_register_failed(&mut self.log, &err);
            return Ok(());
        }
        self.next_conn = self.next_conn.saturating_add(1);
        self.clients.insert(conn, Client::new(fd));
        self.deliver(Event::Accepted { conn, peer })
    }

    /// Readiness on a client socket: the reads or writes its interest
    /// allows, then any error or hang-up, reported whatever the interest
    /// (DRV-10) if the I/O did not already report it.
    pub(super) fn client_event(
        &mut self,
        conn: ConnId,
        readiness: Readiness,
        trouble: Trouble,
    ) -> io::Result<()> {
        let Some(client) = self.clients.get(&conn) else {
            // Closed earlier in this batch, or never ours (DRV-05).
            return Ok(());
        };
        match client.health {
            Health::Ok => {}
            // Its `Failed` is queued or delivered (DRV-15).
            Health::Broken => return Ok(()),
        }
        match client.interest {
            Interest::None => {}
            Interest::Read => {
                if readiness.has_read() {
                    self.client_read(conn)?;
                }
            }
            Interest::Write => {
                if readiness.has_write() {
                    self.client_write(conn)?;
                }
            }
            // Never set for a client.
            Interest::ReadWrite => {
                if readiness.has_read() {
                    self.client_read(conn)?;
                }
                if readiness.has_write() {
                    self.client_write(conn)?;
                }
            }
        }
        match trouble {
            Trouble::None => Ok(()),
            Trouble::Hangup | Trouble::Error | Trouble::ErrorHangup => {
                let Some(client) = self.clients.get(&conn) else {
                    return Ok(());
                };
                // The pending error if there is one; a hang-up without
                // one means the peer is gone all the same.
                let error = match take_error(client.fd.as_fd()) {
                    Ok(Some(err)) => conn_error(&err),
                    Ok(None) => ConnError::Reset,
                    Err(err) => conn_error(&err),
                };
                self.deliver(Event::Failed { conn, error })
            }
        }
    }

    /// Reads into `recv_buffer` until it is empty or the socket would
    /// block (DRV-03), each read reported as the very next call (DRV-04).
    fn client_read(&mut self, conn: ConnId) -> io::Result<()> {
        loop {
            let Some(client) = self.clients.get(&conn) else {
                return Ok(());
            };
            let reading = match (client.health, client.interest) {
                (Health::Ok, Interest::Read | Interest::ReadWrite) => client.eof == Eof::NotYet,
                (Health::Broken, _) | (Health::Ok, Interest::None | Interest::Write) => false,
            };
            if !reading {
                return Ok(());
            }
            let buf = self.core.recv_buffer(conn);
            if buf.is_empty() {
                return Ok(());
            }
            let event = match read(client.fd.as_fd(), buf) {
                Io::Done(0) | Io::WouldBlock => return Ok(()),
                Io::Done(len) => Event::Received { conn, len },
                Io::Closed => {
                    self.client_eof(conn)?;
                    Event::ReadClosed { conn }
                }
                Io::Err(err) => Event::Failed {
                    conn,
                    error: conn_error(&err),
                },
            };
            let more = matches!(event, Event::Received { .. });
            self.deliver(event)?;
            if !more {
                return Ok(());
            }
        }
    }

    /// The end of stream was read: it is reported once, and the socket is
    /// not read again.
    fn client_eof(&mut self, conn: ConnId) -> io::Result<()> {
        if let Some(client) = self.clients.get_mut(&conn) {
            client.eof = Eof::Delivered;
        }
        self.sync_client(conn)
    }

    /// Writes `send_buffer`, then the `send_file` segment, until done or
    /// the socket would block (DRV-03), each write reported as the very
    /// next call (DRV-04).
    fn client_write(&mut self, conn: ConnId) -> io::Result<()> {
        loop {
            let Some(client) = self.clients.get(&conn) else {
                return Ok(());
            };
            match (client.health, client.interest) {
                (Health::Ok, Interest::Write | Interest::ReadWrite) => {}
                (Health::Broken, _) | (Health::Ok, Interest::None | Interest::Read) => {
                    return Ok(());
                }
            }
            let buf = self.core.send_buffer(conn);
            if buf.is_empty() {
                // The send buffer first, then the file segment (DRV-03).
                let Some(event) = self.sendfile_segment(conn) else {
                    return Ok(());
                };
                let more = matches!(event, Event::FileSent { .. });
                self.deliver(event)?;
                if !more {
                    return Ok(());
                }
                continue;
            }
            let event = match write(client.fd.as_fd(), buf) {
                Io::Done(0) | Io::WouldBlock => return Ok(()),
                Io::Done(len) => Event::Sent { conn, len },
                // `send` of a non-empty buffer never returns 0.
                Io::Closed => Event::Failed {
                    conn,
                    error: ConnError::Other,
                },
                Io::Err(err) => Event::Failed {
                    conn,
                    error: conn_error(&err),
                },
            };
            let more = matches!(event, Event::Sent { .. });
            self.deliver(event)?;
            if !more {
                return Ok(());
            }
        }
    }

    pub(super) fn set_interest(&mut self, conn: ConnId, interest: Interest) -> io::Result<()> {
        if let Some(client) = self.clients.get_mut(&conn) {
            client.interest = interest;
        }
        self.sync_client(conn)
    }

    /// Brings the client's registration in line with what it wants. A
    /// failed `epoll_ctl` fails only this client (DRV-15): it is logged,
    /// no more I/O is done on the socket, and its `Failed` is delivered
    /// after the actions of the current call.
    fn sync_client(&mut self, conn: ConnId) -> io::Result<()> {
        let Some(client) = self.clients.get_mut(&conn) else {
            return Ok(());
        };
        let wanted = match client.health {
            Health::Ok => client.wanted(),
            Health::Broken => return Ok(()),
        };
        if wanted == client.registered {
            return Ok(());
        }
        match self
            .epoll
            .modify(client.fd.as_fd(), token(Token::Client(conn))?, wanted)
        {
            Ok(()) => client.registered = wanted,
            Err(err) => self.break_client(conn, &err),
        }
        Ok(())
    }

    /// A client's registration failed (DRV-15): logged, no more I/O, out
    /// of the epoll set (so a core that does not close it at once cannot
    /// make the loop wake for it), its `Failed` queued.
    pub(super) fn break_client(&mut self, conn: ConnId, err: &io::Error) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        // Best effort: if this fails too, the socket stays registered
        // until `Close` drops it.
        let _ = self.epoll.delete(client.fd.as_fd());
        log_register_failed(&mut self.log, err);
        client.health = Health::Broken;
        self.broken.push_back(Broken::Client(conn, conn_error(err)));
    }

    pub(super) fn shutdown_write(&mut self, conn: ConnId) {
        if let Some(client) = self.clients.get(&conn) {
            // A socket the peer already reset fails here (`ENOTCONN`); its
            // error is reported by the next read or wait.
            let _ = shutdown_write(client.fd.as_fd());
        }
    }

    /// Closes the socket (dropping it removes it from the epoll set) and
    /// forgets the id: nothing more is delivered for it (DRV-05). A
    /// descriptor is free again, so paused listeners resume (DRV-16).
    pub(super) fn close(&mut self, conn: ConnId) -> io::Result<()> {
        if self.clients.remove(&conn).is_some() {
            self.descriptor_closed()?;
        }
        Ok(())
    }
}
