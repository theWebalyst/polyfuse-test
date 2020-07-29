//! Establish connection with FUSE kernel driver.

#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use libc::{c_int, c_void, iovec};
use std::{
    cmp,
    ffi::OsStr,
    io::{self, IoSlice, IoSliceMut, Read, Write},
    mem::{self, MaybeUninit},
    os::unix::{
        io::{AsRawFd, IntoRawFd, RawFd},
        net::UnixDatagram,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    ptr,
};

const FUSERMOUNT_PROG: &str = "fusermount";
const FUSE_COMMFD_ENV: &str = "_FUSE_COMMFD";

macro_rules! syscall {
    ($fn:ident ( $($arg:expr),* $(,)* ) ) => {{
        let res = unsafe { libc::$fn($($arg),*) };
        if res == -1 {
            return Err(io::Error::last_os_error());
        }
        res
    }};
}

pub struct Fusermount {
    #[allow(dead_code)]
    pid: c_int,
    reader: UnixDatagram,
    mountpoint: PathBuf,
}

impl AsRawFd for Fusermount {
    fn as_raw_fd(&self) -> RawFd {
        self.reader.as_raw_fd()
    }
}

impl Fusermount {
    pub fn mount(mountpoint: &Path, mountopts: &[&OsStr]) -> io::Result<Self> {
        let (reader, writer) = UnixDatagram::pair()?;

        let pid = syscall! { fork() };
        if pid == 0 {
            drop(reader);
            let writer = writer.into_raw_fd();
            unsafe { libc::fcntl(writer, libc::F_SETFD, 0) };

            let mut fusermount = Command::new(FUSERMOUNT_PROG);
            fusermount.env(FUSE_COMMFD_ENV, writer.to_string());
            fusermount.args(mountopts);
            fusermount.arg("--").arg(mountpoint);

            return Err(fusermount.exec());
        }

        Ok(Fusermount {
            pid,
            reader,
            mountpoint: mountpoint.into(),
        })
    }

    pub fn wait(self) -> io::Result<Connection> {
        let Self {
            reader, mountpoint, ..
        } = self;

        let mut buf = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut c_void,
            iov_len: 1,
        };

        #[repr(C)]
        struct Cmsg {
            header: libc::cmsghdr,
            fd: c_int,
        }
        let mut cmsg = MaybeUninit::<Cmsg>::uninit();

        let mut msg = libc::msghdr {
            msg_name: ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut iov,
            msg_iovlen: 1,
            msg_control: cmsg.as_mut_ptr() as *mut c_void,
            msg_controllen: mem::size_of_val(&cmsg),
            msg_flags: 0,
        };

        syscall! { recvmsg(reader.as_raw_fd(), &mut msg, 0) };

        if msg.msg_controllen < mem::size_of_val(&cmsg) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too short control message length",
            ));
        }
        let cmsg = unsafe { cmsg.assume_init() };

        if cmsg.header.cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "got control message with unknown type",
            ));
        }

        // Unmounting is executed when `reader` is dropped and the connection
        // with `fusermount` is closed.
        let _ = reader.into_raw_fd();

        Ok(Connection {
            fd: cmsg.fd,
            mountpoint: Some(mountpoint),
        })
    }
}

/// A connection with the FUSE kernel driver.
#[derive(Debug)]
pub struct Connection {
    fd: RawFd,
    mountpoint: Option<PathBuf>,
}

impl Connection {
    pub fn try_clone(&self) -> io::Result<Self> {
        let clonefd = syscall! { dup(self.fd) };

        Ok(Self {
            fd: clonefd,
            mountpoint: None,
        })
    }

    pub fn unmount(&mut self) -> io::Result<()> {
        if let Some(mountpoint) = self.mountpoint.take() {
            Command::new(FUSERMOUNT_PROG)
                .args(&["-u", "-q", "-z", "--"])
                .arg(&mountpoint)
                .status()?;
        }
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _e = self.unmount();
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Read for Connection {
    #[inline]
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        (&*self).read(dst)
    }

    #[inline]
    fn read_vectored(&mut self, dst: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        (&*self).read_vectored(dst)
    }
}

impl Read for &Connection {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        let len = syscall! {
            read(
                self.fd,//
                dst.as_mut_ptr() as *mut c_void,
                dst.len(),
            )
        };
        Ok(len as usize)
    }

    fn read_vectored(&mut self, dst: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let len = syscall! {
            readv(
                self.fd,//
                dst.as_mut_ptr() as *mut iovec,
                cmp::min(dst.len(), c_int::max_value() as usize) as c_int,
            )
        };
        Ok(len as usize)
    }
}

impl Write for Connection {
    #[inline]
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        (&*self).write(src)
    }

    #[inline]
    fn write_vectored(&mut self, src: &[IoSlice<'_>]) -> io::Result<usize> {
        (&*self).write_vectored(src)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

impl Write for &Connection {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        let res = syscall! {
            write(
                self.fd,//
                src.as_ptr() as *const c_void,
                src.len(),
            )
        };
        Ok(res as usize)
    }

    fn write_vectored(&mut self, src: &[IoSlice<'_>]) -> io::Result<usize> {
        let res = syscall! {
            writev(
                self.fd,//
                src.as_ptr() as *const iovec,
                cmp::min(src.len(), c_int::max_value() as usize) as c_int,
            )
        };
        Ok(res as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
