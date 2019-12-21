//! Lowlevel interface to handle FUSE requests.

use crate::{
    common::StatFs,
    fs::Filesystem,
    init::ConnectionInfo,
    io::{Buffer, Reader, ReaderExt, Writer, WriterExt},
    kernel::{
        fuse_notify_code, //
        fuse_notify_delete_out,
        fuse_notify_inval_entry_out,
        fuse_notify_inval_inode_out,
        fuse_notify_poll_wakeup_out,
        fuse_notify_retrieve_out,
        fuse_notify_store_out,
        fuse_opcode,
    },
    op::{Operation, OperationKind},
    reply::ReplyWriter,
    util::as_bytes,
};
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::{
    collections::HashSet,
    convert::TryFrom,
    ffi::OsStr,
    io, mem,
    os::unix::ffi::OsStrExt,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

/// FUSE session driver.
#[derive(Debug)]
pub struct Session {
    conn: ConnectionInfo,
    bufsize: usize,
    exited: AtomicBool,
    notify_unique: AtomicU64,
    retrieves: Mutex<HashSet<u64>>,
}

impl Session {
    pub(crate) fn new(conn: ConnectionInfo, bufsize: usize) -> Self {
        Self {
            conn,
            bufsize,
            exited: AtomicBool::new(false),
            notify_unique: AtomicU64::new(0),
            retrieves: Mutex::default(),
        }
    }

    fn exit(&self) {
        self.exited.store(true, Ordering::SeqCst);
    }

    pub(crate) fn exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Returns the information about the FUSE connection.
    pub fn connection_info(&self) -> &ConnectionInfo {
        &self.conn
    }

    /// Returns the buffer size required to receive one request.
    pub fn buffer_size(&self) -> usize {
        self.bufsize
    }

    /// Receive one or more requests from the kernel.
    pub async fn receive<R: ?Sized, B: ?Sized>(
        &self,
        reader: &mut R,
        buf: &mut B,
    ) -> io::Result<Vec<Interrupt>>
    where
        R: Reader<Buffer = B> + Unpin,
        B: Buffer + Unpin,
    {
        let mut interrupts = vec![];
        loop {
            reader.receive_msg(buf).await?;
            match buf.header().opcode() {
                Some(fuse_opcode::FUSE_INTERRUPT) => (),
                _ => return Ok(interrupts),
            }

            let (header, kind, data) = buf.extract();
            let kind = OperationKind::parse(header, kind, data)?;
            match kind {
                OperationKind::Interrupt { arg } => {
                    tracing::debug!("Receive INTERRUPT (unique = {:?})", arg.unique);
                    interrupts.push(Interrupt {
                        unique: arg.unique,
                        interrupt_unique: header.unique(),
                    });
                }
                _ => unreachable!(),
            }
        }
    }

    /// Process an incoming request using the specified filesystem operations.
    pub async fn process<F: ?Sized, W: ?Sized, B: ?Sized>(
        &self,
        fs: &F,
        buf: &mut B,
        writer: &mut W,
    ) -> io::Result<()>
    where
        F: Filesystem<B::Data>,
        W: Writer + Send + Unpin,
        B: Buffer + Unpin,
        B::Data: Send,
    {
        if self.exited() {
            tracing::warn!("The sesson has already been exited");
            return Ok(());
        }

        let (header, kind, data) = buf.extract();
        let kind = OperationKind::parse(header, kind, data)?;

        tracing::debug!(
            "Handle a request: unique={}, opcode={:?}",
            header.unique(),
            header.opcode(),
        );

        let mut writer = ReplyWriter::new(header.unique(), &mut *writer);

        match kind {
            OperationKind::Destroy => {
                self.exit();
                return Ok(());
            }
            OperationKind::Operation(op) => match op {
                op @ Operation::Statfs(..) => {
                    fs.reply(op, &mut writer).await?;
                    if !writer.replied() {
                        let mut st = StatFs::default();
                        st.set_namelen(255);
                        st.set_bsize(512);
                        let out = crate::reply::ReplyStatfs::new(st);
                        writer
                            .reply_raw(&[unsafe { crate::util::as_bytes(&out) }])
                            .await?;
                    }
                }
                op => {
                    fs.reply(op, &mut writer).await?;
                    if !writer.replied() {
                        writer.reply_err(libc::ENOSYS).await?;
                    }
                }
            },
            OperationKind::Forget(forgets) => {
                // no reply.
                return fs.forget(forgets.as_ref()).await;
            }
            OperationKind::NotifyReply(op, data) => {
                if self.retrieves.lock().remove(&op.unique()) {
                    return fs.notify_reply(op, data).await;
                }
            }

            OperationKind::Init { .. } => {
                tracing::warn!("ignore an INIT request after initializing the session");
                writer.reply_err(libc::EIO).await?;
            }
            OperationKind::Interrupt { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unexpected request kind",
                ));
            }

            OperationKind::Unknown => {
                tracing::warn!("unsupported opcode: {:?}", header.opcode());
                writer.reply_err(libc::ENOSYS).await?;
            }
        }

        Ok(())
    }

    /// Notify the cache invalidation about an inode to the kernel.
    pub async fn notify_inval_inode<W: ?Sized>(
        &self,
        writer: &mut W,
        ino: u64,
        off: i64,
        len: i64,
    ) -> io::Result<()>
    where
        W: Writer + Unpin,
    {
        if self.exited() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "session is closed",
            ));
        }

        let out = fuse_notify_inval_inode_out {
            ino,
            off,
            len,
            ..Default::default()
        };
        send_notify(
            writer,
            fuse_notify_code::FUSE_NOTIFY_INVAL_INODE,
            &[unsafe { as_bytes(&out) }],
        )
        .await
    }

    /// Notify the invalidation about a directory entry to the kernel.
    pub async fn notify_inval_entry<W: ?Sized>(
        &self,
        writer: &mut W,
        parent: u64,
        name: impl AsRef<OsStr>,
    ) -> io::Result<()>
    where
        W: Writer + Unpin,
    {
        if self.exited() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "session is closed",
            ));
        }

        let name = name.as_ref();
        let namelen = u32::try_from(name.len()).unwrap();
        let out = fuse_notify_inval_entry_out {
            parent,
            namelen,
            ..Default::default()
        };
        send_notify(
            writer,
            fuse_notify_code::FUSE_NOTIFY_INVAL_ENTRY,
            &[unsafe { as_bytes(&out) }, name.as_bytes(), &[0]],
        )
        .await
    }

    /// Notify the invalidation about a directory entry to the kernel.
    ///
    /// The role of this notification is similar to `notify_inval_entry`.
    /// Additionally, when the provided `child` inode matches the inode
    /// in the dentry cache, the inotify will inform the deletion to
    /// watchers if exists.
    pub async fn notify_delete<W: ?Sized>(
        &self,
        writer: &mut W,
        parent: u64,
        child: u64,
        name: impl AsRef<OsStr>,
    ) -> io::Result<()>
    where
        W: Writer + Unpin,
    {
        if self.exited() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "session is closed",
            ));
        }

        let name = name.as_ref();
        let namelen = u32::try_from(name.len()).unwrap();
        let out = fuse_notify_delete_out {
            parent,
            child,
            namelen,
            ..Default::default()
        };
        send_notify(
            writer,
            fuse_notify_code::FUSE_NOTIFY_DELETE,
            &[unsafe { as_bytes(&out) }, name.as_bytes(), &[0]],
        )
        .await
    }

    /// Push the data in an inode for updating the kernel cache.
    pub async fn notify_store<W: ?Sized>(
        &self,
        writer: &mut W,
        ino: u64,
        offset: u64,
        data: &[&[u8]],
    ) -> io::Result<()>
    where
        W: Writer + Unpin,
    {
        if self.exited() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "session is closed",
            ));
        }

        let size = u32::try_from(data.iter().map(|t| t.len()).sum::<usize>()).unwrap();
        let out = fuse_notify_store_out {
            nodeid: ino,
            offset,
            size,
            ..Default::default()
        };
        let data: SmallVec<[_; 4]> = Some(unsafe { as_bytes(&out) })
            .into_iter()
            .chain(data.iter().copied())
            .collect();
        send_notify(writer, fuse_notify_code::FUSE_NOTIFY_STORE, &*data).await
    }

    /// Retrieve data in an inode from the kernel cache.
    pub async fn notify_retrieve<W: ?Sized>(
        &self,
        writer: &mut W,
        ino: u64,
        offset: u64,
        size: u32,
    ) -> io::Result<u64>
    where
        W: Writer + Unpin,
    {
        if self.exited() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "session is closed",
            ));
        }

        let unique = self.notify_unique.fetch_add(1, Ordering::SeqCst);
        self.retrieves.lock().insert(unique);

        let out = fuse_notify_retrieve_out {
            notify_unique: unique,
            nodeid: ino,
            offset,
            size,
            ..Default::default()
        };
        send_notify(
            writer,
            fuse_notify_code::FUSE_NOTIFY_RETRIEVE,
            &[unsafe { as_bytes(&out) }],
        )
        .await?;

        Ok(unique)
    }

    /// Send I/O readiness to the kernel.
    pub async fn notify_poll_wakeup<W: ?Sized>(&self, writer: &mut W, kh: u64) -> io::Result<()>
    where
        W: Writer + Unpin,
    {
        if self.exited() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "session is closed",
            ));
        }

        let out = fuse_notify_poll_wakeup_out {
            kh,
            ..Default::default()
        };
        send_notify(
            writer,
            fuse_notify_code::FUSE_NOTIFY_POLL,
            &[unsafe { as_bytes(&out) }],
        )
        .await
    }
}

/// Information about an interrupt request.
#[derive(Debug, Copy, Clone)]
pub struct Interrupt {
    unique: u64,
    interrupt_unique: u64,
}

impl Interrupt {
    /// Return the unique ID of the interrupted request.
    ///
    /// The implementation of FUSE filesystem deamon library should
    /// handle this interrupt request by sending a reply with an EINTR error,
    /// and terminate the background task processing the corresponding request.
    #[inline]
    pub fn unique(&self) -> u64 {
        self.unique
    }

    /// Return the unique ID of the interrupt request itself.
    ///
    /// If the filesystem daemon is not ready to handle this interrupt
    /// request, sending a reply using this unique with EAGAIN error causes
    /// the kernel to requeue its interrupt request.
    #[inline]
    pub fn interrupt_unique(&self) -> u64 {
        self.interrupt_unique
    }
}

#[inline]
async fn send_notify<W: ?Sized>(
    writer: &mut W,
    code: fuse_notify_code,
    data: &[&[u8]],
) -> io::Result<()>
where
    W: Writer + Unpin,
{
    let code = unsafe { mem::transmute::<_, i32>(code) };
    writer.send_msg(0, code, data).await
}
