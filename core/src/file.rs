//! Files as the core sees them: ids, metadata and the ranges the driver
//! sends. The core never reads file bytes; the driver opens, reads ahead and
//! `sendfile`s on its behalf (see the contract in the `core` module).

use std::collections::BTreeMap;

use crate::config::OnFileChange;
use crate::core::ConnId;

/// Opaque file id, allocated by the core (sequentially from 1) and never
/// reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u64);

/// Index into [`crate::Core::static_roots`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootId(pub u32);

/// What an opened path turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Directory,
    /// Device, socket, FIFO or anything else that is neither.
    Other,
}

/// What identifies a version of a file: its size and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub size: u64,
    pub modified_ns: u64,
}

/// Metadata of an opened file, taken from the open descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub kind: FileKind,
    pub stamp: FileStamp,
}

/// Why a file operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileError {
    /// No such file (also: a path component is not a directory).
    NotFound,
    /// A symbolic link somewhere in the path.
    Symlink,
    PermissionDenied,
    /// `sendfile` hit end of file before the segment ended.
    Truncated,
    /// The file changed in a way the driver detected itself.
    Changed,
    Other,
}

/// A byte range of an open file the driver may `sendfile` now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSegment {
    pub file: FileId,
    pub offset: u64,
    pub len: u64,
}

/// How much of a file one `Readahead` covers.
pub(crate) const FILE_CHUNK: u64 = 256 * 1024;

/// Where a file being sent is between disk and socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chunk {
    /// A `Readahead` up to `end` is in flight.
    Reading { end: u64 },
    /// The bytes up to `end` are in the page cache, and the file's stamp
    /// when they were read passed the route's change policy.
    Ready { end: u64 },
    /// Every byte was sent (or the file is empty): done once the send
    /// buffer is drained.
    Done,
}

/// Whether a file whose stamp was `opened` at open and is `now` at a
/// chunk's read may still be sent under `policy`; the error the transfer is
/// aborted with otherwise. Growth allowed under `AllowGrowth` is sent only
/// up to the size at open.
pub(crate) fn check_stamp(
    policy: OnFileChange,
    opened: FileStamp,
    now: FileStamp,
) -> Result<(), FileError> {
    if now == opened {
        return Ok(());
    }
    match policy {
        OnFileChange::Abort => Err(FileError::Changed),
        OnFileChange::AllowGrowth => match now.size.cmp(&opened.size) {
            std::cmp::Ordering::Greater => Ok(()),
            // Same size, other modification time: rewritten in place.
            std::cmp::Ordering::Equal => Err(FileError::Changed),
            std::cmp::Ordering::Less => Err(FileError::Truncated),
        },
    }
}

/// Who a file id the core handed out belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileUse {
    /// Being opened or open for this connection.
    Conn(ConnId),
    /// The `OpenFile` is unanswered but its connection is gone: a
    /// `FileOpened` gets `CloseFile`, a `FileFailed` is dropped. It holds
    /// a `max_connections` slot until then.
    Orphan,
}

/// The core's file ids: allocation and ownership.
#[derive(Debug)]
pub(crate) struct FileTable {
    next: u64,
    owners: BTreeMap<FileId, FileUse>,
}

/// What a completion event's file id refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owner {
    Conn(ConnId),
    Orphan,
    /// Never handed out, or already closed: the event is ignored.
    Unknown,
}

impl FileTable {
    pub(crate) fn new() -> FileTable {
        FileTable {
            next: 1,
            owners: BTreeMap::new(),
        }
    }

    /// A fresh id, owned by `conn`. The `u64` cannot run out in practice
    /// (a billion opens per second would take over 500 years); should it
    /// ever, the saturated id would be reused, which the driver contract
    /// forbids.
    pub(crate) fn allocate(&mut self, conn: ConnId) -> FileId {
        let file = FileId(self.next);
        self.next = self.next.saturating_add(1);
        self.owners.insert(file, FileUse::Conn(conn));
        file
    }

    pub(crate) fn owner(&self, file: FileId) -> Owner {
        match self.owners.get(&file) {
            Some(FileUse::Conn(conn)) => Owner::Conn(*conn),
            Some(FileUse::Orphan) => Owner::Orphan,
            None => Owner::Unknown,
        }
    }

    /// The connection is gone while the open is in flight.
    pub(crate) fn orphan(&mut self, file: FileId) {
        if let Some(owner) = self.owners.get_mut(&file) {
            *owner = FileUse::Orphan;
        }
    }

    /// The id is done with: closed, or its open failed.
    pub(crate) fn release(&mut self, file: FileId) {
        self.owners.remove(&file);
    }

    /// Ids still tracked (for tests).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.owners.len()
    }
}
