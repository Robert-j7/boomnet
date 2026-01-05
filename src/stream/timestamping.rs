//! Linux RX timestamping wrapper stream (SCM_TIMESTAMPING).
#![cfg(target_os = "linux")]

use crate::service::select::Selectable;
use crate::stream::{ConnectionInfo, ConnectionInfoProvider, RxTimestamped, RxTimestamps};
#[cfg(feature = "mio")]
use mio::event::Source;
#[cfg(feature = "mio")]
use mio::{Interest, Registry, Token};
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, RawFd};
use std::ptr;

// ---- linux/net_tstamp.h flags ----
const SOF_TIMESTAMPING_RX_HARDWARE: libc::c_int = 1 << 2;
const SOF_TIMESTAMPING_RX_SOFTWARE: libc::c_int = 1 << 3;
const SOF_TIMESTAMPING_SOFTWARE: libc::c_int = 1 << 4;
// Optional: driver/kernel may provide HW time converted into system time domain.
const SOF_TIMESTAMPING_SYS_HARDWARE: libc::c_int = 1 << 5;
const SOF_TIMESTAMPING_RAW_HARDWARE: libc::c_int = 1 << 6;

const SCM_TIMESTAMPING: libc::c_int = libc::SO_TIMESTAMPING;

#[repr(C)]
#[derive(Clone, Copy)]
struct ScmTimestamping {
    ts: [libc::timespec; 3],
}

#[repr(align(8))]
#[derive(Debug)]
struct CtrlBuf([u8; 512]);

#[inline]
fn ns_from_timespec(ts: libc::timespec) -> u64 {
    if ts.tv_sec == 0 && ts.tv_nsec == 0 {
        return 0;
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64)
}

#[inline]
fn last_err() -> io::Error {
    io::Error::last_os_error()
}

// --- CMSG helpers ---
#[inline]
fn cmsg_align(len: usize) -> usize {
    let a = mem::size_of::<libc::c_long>();
    (len + a - 1) & !(a - 1)
}

unsafe fn cmsg_firsthdr(msg: *const libc::msghdr) -> *mut libc::cmsghdr {
    unsafe {
        if (*msg).msg_controllen as usize >= mem::size_of::<libc::cmsghdr>() {
            (*msg).msg_control as *mut libc::cmsghdr
        } else {
            ptr::null_mut()
        }
    }
}

unsafe fn cmsg_nxthdr(msg: *const libc::msghdr, cmsg: *const libc::cmsghdr) -> *mut libc::cmsghdr {
    unsafe {
        if cmsg.is_null() {
            return ptr::null_mut();
        }
        let base = (*msg).msg_control as *const u8;
        let end = base.add((*msg).msg_controllen as usize);

        let cur = cmsg as *const u8;
        let next = cur.add(cmsg_align((*cmsg).cmsg_len as usize));

        if next.add(mem::size_of::<libc::cmsghdr>()) > end {
            return ptr::null_mut();
        }
        let next_cmsg = next as *const libc::cmsghdr;
        let next_len = (*next_cmsg).cmsg_len as usize;
        if next.add(next_len) > end {
            return ptr::null_mut();
        }
        next as *mut libc::cmsghdr
    }
}

unsafe fn cmsg_data(cmsg: *const libc::cmsghdr) -> *const u8 {
    unsafe { (cmsg as *const u8).add(cmsg_align(mem::size_of::<libc::cmsghdr>())) }
}

/// Enable RX timestamping on an already-created socket.
pub fn enable_rx_timestamping(fd: RawFd) -> io::Result<()> {
    let flags: libc::c_int = SOF_TIMESTAMPING_RX_HARDWARE
        | SOF_TIMESTAMPING_RAW_HARDWARE
        | SOF_TIMESTAMPING_SYS_HARDWARE
        | SOF_TIMESTAMPING_RX_SOFTWARE
        | SOF_TIMESTAMPING_SOFTWARE;

    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TIMESTAMPING,
            (&flags as *const libc::c_int).cast(),
            mem::size_of_val(&flags) as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(last_err())
    } else {
        Ok(())
    }
}

/// Wraps any stream and captures SCM_TIMESTAMPING on reads via recvmsg().
#[derive(Debug)]
pub struct TimestampingStream<S> {
    inner: S,
    ctrl: CtrlBuf,
    last: Option<RxTimestamps>,
}

impl<S> TimestampingStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            ctrl: CtrlBuf([0u8; 512]),
            last: None,
        }
    }

    pub fn inner_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    pub fn inner_ref(&self) -> &S {
        &self.inner
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: AsRawFd> AsRawFd for TimestampingStream<S> {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl<S: AsRawFd> RxTimestamped for TimestampingStream<S> {
    fn last_rx_timestamps(&self) -> Option<RxTimestamps> {
        self.last
    }

    fn take_last_rx_timestamps(&mut self) -> Option<RxTimestamps> {
        self.last.take()
    }
}

impl<S: AsRawFd> Read for TimestampingStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let fd = self.inner.as_raw_fd();

            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr().cast::<libc::c_void>(),
                iov_len: buf.len(),
            };

            let mut msg: libc::msghdr = mem::zeroed();
            msg.msg_iov = &mut iov as *mut libc::iovec;
            msg.msg_iovlen = 1;
            msg.msg_control = self.ctrl.0.as_mut_ptr().cast::<libc::c_void>();
            msg.msg_controllen = self.ctrl.0.len() as libc::size_t;

            let n = libc::recvmsg(fd, &mut msg as *mut libc::msghdr, 0);
            if n < 0 {
                return Err(last_err());
            }
            if n == 0 {
                self.last = None;
                return Ok(0);
            }

            self.last = None;
            let mut out = RxTimestamps::default();
            let mut c = cmsg_firsthdr(&msg as *const libc::msghdr);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == SCM_TIMESTAMPING {
                    let hdr = cmsg_align(mem::size_of::<libc::cmsghdr>());
                    let need = mem::size_of::<ScmTimestamping>();
                    let have = (*c).cmsg_len as usize;
                    if have >= hdr + need {
                        let tp = cmsg_data(c).cast::<ScmTimestamping>();
                        let t = *tp;
                        out.sw_ns = ns_from_timespec(t.ts[0]);
                        out.hw_sys_ns = ns_from_timespec(t.ts[1]);
                        out.hw_raw_ns = ns_from_timespec(t.ts[2]);
                        self.last = Some(out);
                    }
                    break;
                }
                c = cmsg_nxthdr(&msg as *const libc::msghdr, c as *const libc::cmsghdr);
            }

            Ok(n as usize)
        }
    }
}

impl<S: Write> Write for TimestampingStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<S: ConnectionInfoProvider> ConnectionInfoProvider for TimestampingStream<S> {
    fn connection_info(&self) -> &ConnectionInfo {
        self.inner.connection_info()
    }
}

impl<S: Selectable> Selectable for TimestampingStream<S> {
    fn connected(&mut self) -> io::Result<bool> {
        self.inner.connected()
    }

    fn make_writable(&mut self) -> io::Result<()> {
        self.inner.make_writable()
    }

    fn make_readable(&mut self) -> io::Result<()> {
        self.inner.make_readable()
    }
}

#[cfg(feature = "mio")]
impl<S: Source> Source for TimestampingStream<S> {
    fn register(&mut self, registry: &Registry, token: Token, interests: Interest) -> io::Result<()> {
        registry.register(&mut self.inner, token, interests)
    }

    fn reregister(&mut self, registry: &Registry, token: Token, interests: Interest) -> io::Result<()> {
        registry.reregister(&mut self.inner, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        registry.deregister(&mut self.inner)
    }
}
