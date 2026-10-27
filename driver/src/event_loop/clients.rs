//! Client connections: accepting them, reading into the core's receive
//! buffer, writing from its send buffer, and the client actions.

use std::io;
use std::os::fd::AsFd;

use durchlauferhitzer_core::{ConnError, ConnId, Event, Interest};

use super::{Accepting, Driver, conn_error, token};
use crate::conns::{Client, Eof, Token, has_read, has_write};
use crate::log::{DriverLog, LogEntry};
use crate::sys::epoll::{Readiness, Trouble};
use crate::sys::fd::Io;
use crate::sys::socket::{
    accept_nonblocking, read, set_nodelay, shutdown_write, take_error, write,
};

/// What an `accept` error means for the loop.
enum AcceptError {
    /// The connection went away before it was taken (`ECONNABORTED`,
    /// `EPROTO`) or a firewall refused it (`EPERM`): skip it and go on.
    Transient,
    /// Out of descriptors (`EMFILE`, `ENFILE`): log it and pause the
    /// listeners until a connection closes.
    OutOfDescriptors(i32),
    /// Anything else (`ENOBUFS`, `ENOMEM`, ...): log it and stop for this
    /// turn.
    Other(i32),
}

fn classify(err: &io::Error) -> AcceptError {
    match err.raw_os_error() {
        Some(libc::ECONNABORTED | libc::EPROTO | libc::EPERM) => AcceptError::Transient,
        Some(errno @ (libc::EMFILE | libc::ENFILE)) => AcceptError::OutOfDescriptors(errno),
        Some(errno) => AcceptError::Other(errno),
        None => AcceptError::Other(0),
    }
}

impl Driver {
    /// Accepts every pending connection on listener `i`, until it would
    /// block.
    pub(super) fn accept_all(&mut self, i: u32) -> io::Result<()> {
        loop {
            match self.accepting {
                Accepting::Open => {}
                Accepting::Paused => return Ok(()),
            }
            let Some(listener) = usize::try_from(i).ok().and_then(|i| self.listeners.get(i)) else {
                return Ok(());
            };
            match accept_nonblocking(listener.as_fd()) {
                Ok(Some((fd, peer))) => self.accepted(fd, peer.ip())?,
                Ok(None) => return Ok(()),
                Err(err) => match classify(&err) {
                    AcceptError::Transient => {}
                    AcceptError::OutOfDescriptors(errno) => {
                        self.log
                            .write(LogEntry::Driver(DriverLog::AcceptFailed { errno }));
                        return self.set_accepting(Accepting::Paused);
                    }
                    AcceptError::Other(errno) => {
                        self.log
                            .write(LogEntry::Driver(DriverLog::AcceptFailed { errno }));
                        return Ok(());
                    }
                },
            }
        }
    }

    /// Takes the listeners out of the wait, or puts them back.
    fn set_accepting(&mut self, accepting: Accepting) -> io::Result<()> {
        if self.accepting == accepting {
            return Ok(());
        }
        self.accepting = accepting;
        let readiness = match accepting {
            Accepting::Open => Readiness::Read,
            Accepting::Paused => Readiness::None,
        };
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
    fn accepted(&mut self, fd: std::os::fd::OwnedFd, peer: std::net::IpAddr) -> io::Result<()> {
        // Cannot fail on a freshly accepted TCP socket; a socket that
        // somehow refuses it still works.
        let _ = set_nodelay(fd.as_fd());
        let conn = ConnId(self.next_conn);
        // 2^61 connections are never reached; the socket is dropped then.
        let Some(raw) = Token::Client(conn).encode() else {
            return Ok(());
        };
        if let Err(err) = self.epoll.add(fd.as_fd(), raw, Readiness::None) {
            // The core never heard of it: drop the socket, log the reason.
            let errno = err.raw_os_error().unwrap_or(0);
            self.log
                .write(LogEntry::Driver(DriverLog::AcceptFailed { errno }));
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
        match client.interest {
            Interest::None => {}
            Interest::Read => {
                if has_read(readiness) {
                    self.client_read(conn)?;
                }
            }
            Interest::Write => {
                if has_write(readiness) {
                    self.client_write(conn)?;
                }
            }
            // Never set for a client.
            Interest::ReadWrite => {
                if has_read(readiness) {
                    self.client_read(conn)?;
                }
                if has_write(readiness) {
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
            let reading = match client.interest {
                Interest::Read | Interest::ReadWrite => client.eof == Eof::NotYet,
                Interest::None | Interest::Write => false,
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
            match client.interest {
                Interest::Write | Interest::ReadWrite => {}
                Interest::None | Interest::Read => return Ok(()),
            }
            let buf = self.core.send_buffer(conn);
            if buf.is_empty() {
                // `send_file` segments need the disk pool (Task 6); with
                // no static roots there are none.
                return Ok(());
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

    /// Brings the client's registration in line with what it wants.
    fn sync_client(&mut self, conn: ConnId) -> io::Result<()> {
        let Some(client) = self.clients.get_mut(&conn) else {
            return Ok(());
        };
        let wanted = client.wanted();
        if wanted != client.registered {
            self.epoll
                .modify(client.fd.as_fd(), token(Token::Client(conn))?, wanted)?;
            client.registered = wanted;
        }
        Ok(())
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
    /// descriptor is free again, so paused listeners resume.
    pub(super) fn close(&mut self, conn: ConnId) -> io::Result<()> {
        if self.clients.remove(&conn).is_some() {
            self.set_accepting(Accepting::Open)?;
        }
        Ok(())
    }
}
