// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M3: unix SOCK_SEQPACKET sockets with file-descriptor passing (SCM_RIGHTS) and peer credentials, for the HID
//! helper (root) <-> daemon (user) link. One packet = one text message (+ fds).

use libc::{c_int, c_void};
use std::ffi::CString;

pub fn errstr() -> String {
    crate::sys::strerr(crate::sys::errno())
}

fn sockaddr(path: &str) -> Result<(libc::sockaddr_un, libc::socklen_t), String> {
    let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    a.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let b = path.as_bytes();
    if b.len() >= a.sun_path.len() {
        return Err(format!("socket path too long: {}", path));
    }
    for (i, c) in b.iter().enumerate() {
        a.sun_path[i] = *c as libc::c_char;
    }
    Ok((a, (std::mem::size_of::<libc::sa_family_t>() + b.len() + 1) as libc::socklen_t))
}

/// a listening seqpacket socket at `path` (an old socket file is replaced), mode 0666 (the helper checks the peer)
pub fn listen(path: &str) -> Result<c_int, String> {
    let _ = std::fs::remove_file(path);
    if let Some(d) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {}", d.display(), e))?;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(format!("socket: {}", errstr()));
    }
    let (a, l) = sockaddr(path)?;
    if unsafe { libc::bind(fd, &a as *const _ as *const libc::sockaddr, l) } < 0 {
        let e = errstr();
        unsafe { libc::close(fd) };
        return Err(format!("bind {}: {}", path, e));
    }
    let c = CString::new(path).unwrap();
    unsafe { libc::chmod(c.as_ptr(), 0o666) };
    if unsafe { libc::listen(fd, 4) } < 0 {
        return Err(format!("listen: {}", errstr()));
    }
    Ok(fd)
}

pub fn connect(path: &str) -> Result<c_int, String> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(format!("socket: {}", errstr()));
    }
    let (a, l) = sockaddr(path)?;
    if unsafe { libc::connect(fd, &a as *const _ as *const libc::sockaddr, l) } < 0 {
        let e = errstr();
        unsafe { libc::close(fd) };
        return Err(format!("connect {}: {}", path, e));
    }
    Ok(fd)
}

pub fn accept(fd: c_int) -> Option<c_int> {
    let c = unsafe { libc::accept4(fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
    (c >= 0).then_some(c)
}

/// (pid, uid) of the peer
pub fn peer(fd: c_int) -> Option<(i32, u32)> {
    let mut cr: libc::ucred = unsafe { std::mem::zeroed() };
    let mut l = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cr as *mut _ as *mut c_void, &mut l) };
    (r == 0).then_some((cr.pid, cr.uid))
}

/// send one message with fds
pub fn send(fd: c_int, msg: &str, fds: &[c_int]) -> Result<(), String> {
    let mut iov = libc::iovec { iov_base: msg.as_ptr() as *mut c_void, iov_len: msg.len() };
    let space = unsafe { libc::CMSG_SPACE((fds.len() * std::mem::size_of::<c_int>()) as u32) } as usize;
    let mut cbuf = vec![0u8; space.max(1)];
    let mut m: libc::msghdr = unsafe { std::mem::zeroed() };
    m.msg_iov = &mut iov;
    m.msg_iovlen = 1;
    if !fds.is_empty() {
        m.msg_control = cbuf.as_mut_ptr() as *mut c_void;
        m.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&m);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((fds.len() * std::mem::size_of::<c_int>()) as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c) as *mut c_int, fds.len());
        }
    }
    let r = unsafe { libc::sendmsg(fd, &m, libc::MSG_NOSIGNAL) };
    if r < 0 {
        return Err(format!("sendmsg: {}", errstr()));
    }
    Ok(())
}

/// receive one message (None = EOF / error); received fds are close-on-exec
pub fn recv(fd: c_int) -> Option<(String, Vec<c_int>)> {
    let mut buf = vec![0u8; 8192];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut c_void, iov_len: buf.len() };
    let space = unsafe { libc::CMSG_SPACE((8 * std::mem::size_of::<c_int>()) as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    let mut m: libc::msghdr = unsafe { std::mem::zeroed() };
    m.msg_iov = &mut iov;
    m.msg_iovlen = 1;
    m.msg_control = cbuf.as_mut_ptr() as *mut c_void;
    m.msg_controllen = space as _;
    let n = unsafe { libc::recvmsg(fd, &mut m, libc::MSG_CMSG_CLOEXEC) };
    if n <= 0 {
        return None;
    }
    let mut fds = vec![];
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&m);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let cnt = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<c_int>();
                let p = libc::CMSG_DATA(c) as *const c_int;
                for i in 0..cnt {
                    fds.push(*p.add(i));
                }
            }
            c = libc::CMSG_NXTHDR(&m, c);
        }
    }
    Some((String::from_utf8_lossy(&buf[..n as usize]).into_owned(), fds))
}

/// write a whole buffer to a (non-blocking) fd once; Ok(n) or the errno
pub fn write_fd(fd: c_int, b: &[u8]) -> Result<usize, i32> {
    let r = unsafe { libc::write(fd, b.as_ptr() as *const c_void, b.len()) };
    if r < 0 {
        Err(crate::sys::errno())
    } else {
        Ok(r as usize)
    }
}

pub fn read_fd(fd: c_int, b: &mut [u8]) -> Result<usize, i32> {
    let r = unsafe { libc::read(fd, b.as_mut_ptr() as *mut c_void, b.len()) };
    if r < 0 {
        Err(crate::sys::errno())
    } else {
        Ok(r as usize)
    }
}

pub fn close(fd: c_int) {
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

pub fn set_nonblock(fd: c_int) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, f | libc::O_NONBLOCK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seqpacket_roundtrip_with_fds_and_peer() {
        let p = format!("/tmp/dexhid-ipc-test-{}.sock", std::process::id());
        let l = listen(&p).unwrap();
        let c = connect(&p).unwrap();
        let s = accept(l).unwrap();
        assert_eq!(peer(s).map(|x| x.1), Some(unsafe { libc::getuid() }));
        let mut pipe = [0; 2];
        unsafe { libc::pipe(pipe.as_mut_ptr()) };
        send(s, "OK test", &[pipe[1]]).unwrap();
        let (m, fds) = recv(c).unwrap();
        assert_eq!(m, "OK test");
        assert_eq!(fds.len(), 1);
        assert_eq!(write_fd(fds[0], b"hi"), Ok(2), "the passed fd is the pipe's write end");
        let mut b = [0u8; 4];
        assert_eq!(read_fd(pipe[0], &mut b), Ok(2));
        send(c, "HELLO", &[]).unwrap();
        assert_eq!(recv(s).unwrap(), ("HELLO".to_string(), vec![]));
        close(c);
        assert!(recv(s).is_none(), "EOF when the peer is gone");
        for f in [l, s, pipe[0], pipe[1], fds[0]] {
            close(f);
        }
        let _ = std::fs::remove_file(&p);
    }
}
