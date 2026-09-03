//! Descriptor transport for screenshot and screencast payloads.
//!
//! Every payload crosses the socket as one SCM_RIGHTS descriptor behind a
//! `0xfd` marker byte. SHM payloads are sealed memfds whose contents the
//! sender can no longer modify; dmabuf payloads are single-plane GPU
//! buffers, which cannot carry memfd seals and are validated by size only.
//! A dmabuf has a fixed size for its lifetime, so the size check bounds the
//! receiver's mapping the same way the seal check does for memfds. The
//! transport flows server-to-client: captures and stream frames.

use std::fs::File;
#[cfg(any(feature = "test-server", test))]
use std::io::Write;
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

const BLOB_MARKER: u8 = 0xfd;
pub(crate) const MAX_BLOB_BYTES: u64 = 288 * 1024 * 1024;
const REQUIRED_SEALS: libc::c_int =
    libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;

/// A sealed, immutable in-memory payload ready to send. The sender seals
/// before the descriptor crosses the socket, so the receiver can trust the
/// contents never change underneath it. Only the test server sends payloads
/// (captures, frames); the client is receive-only.
#[cfg(any(feature = "test-server", test))]
pub(crate) struct SealedBlob {
    file: File,
    len: u64,
}

#[cfg(any(feature = "test-server", test))]
impl SealedBlob {
    pub(crate) fn new(bytes: &[u8]) -> io::Result<Self> {
        let len = u64::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "blob length overflow"))?;
        validate_len(len)?;
        // SAFETY: the name is static and NUL-terminated; the returned fd is
        // checked before ownership is constructed.
        let fd = unsafe {
            libc::memfd_create(
                c"atrium-portal-ipc".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `memfd_create` returned a new owned descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes)?;
        file.seek(SeekFrom::Start(0))?;
        // SAFETY: `file` owns a memfd created with sealing enabled.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, REQUIRED_SEALS) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file, len })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn send(&self, stream: &UnixStream) -> io::Result<()> {
        send_fd(stream, self.file.as_raw_fd())
    }
}

pub(crate) fn receive(stream: &UnixStream, expected_len: u64) -> io::Result<Vec<u8>> {
    let mut file = receive_memfd_file(stream, expected_len)?;
    let length = usize::try_from(expected_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "capture is too large"))?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Receive a sealed memfd and return it positioned at offset 0. The seals
/// freeze the payload before the descriptor crosses the socket, so the
/// receiver may safely memory-map it instead of copying.
pub(crate) fn receive_memfd_file(stream: &UnixStream, expected_len: u64) -> io::Result<File> {
    let file = receive_validated_fd(stream, expected_len)?;
    // SAFETY: F_GET_SEALS has no pointer argument and `file` owns a valid fd.
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 || seals & REQUIRED_SEALS != REQUIRED_SEALS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "capture descriptor is not fully sealed",
        ));
    }
    Ok(file)
}

/// Receive a single-plane dmabuf descriptor. dmabufs cannot carry memfd
/// seals and do not stat as regular files; the allocation size floor is the
/// only integrity property the receiver can check. Contents remain
/// GPU-owned and may change between frames, which is inherent to dmabuf
/// sharing.
pub(crate) fn receive_dmabuf_file(stream: &UnixStream, expected_len: u64) -> io::Result<File> {
    validate_len(expected_len)?;
    let fd = receive_fd(stream)?;
    // SAFETY: `receive_fd` returns a newly received owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    // The announced plane size is stride*height; the allocation behind the
    // descriptor may be larger (GPU granularity), so only a short
    // descriptor is invalid.
    if metadata.len() < expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "capture descriptor is shorter than announced (expected {expected_len}, got {})",
                metadata.len()
            ),
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn receive_validated_fd(stream: &UnixStream, expected_len: u64) -> io::Result<File> {
    validate_len(expected_len)?;
    let fd = receive_fd(stream)?;
    // SAFETY: `receive_fd` returns a newly received owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "capture descriptor is not a regular file of {expected_len} bytes \
                 (regular file: {}, length: {})",
                metadata.file_type().is_file(),
                metadata.len()
            ),
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn validate_len(length: u64) -> io::Result<()> {
    if length == 0 || length > MAX_BLOB_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("blob length {length} is outside 1..={MAX_BLOB_BYTES}"),
        ));
    }
    Ok(())
}

#[cfg(any(feature = "test-server", test))]
pub(crate) fn send_fd(stream: &UnixStream, fd: RawFd) -> io::Result<()> {
    let mut marker = BLOB_MARKER;
    let mut iov = libc::iovec {
        iov_base: (&mut marker as *mut u8).cast(),
        iov_len: 1,
    };
    // SAFETY: CMSG_SPACE computes storage for exactly one descriptor.
    let control_len = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as libc::c_uint) } as usize;
    let mut control = vec![0_u8; control_len];
    // SAFETY: all pointers reference live storage for the duration of sendmsg.
    let sent = unsafe {
        let mut message: libc::msghdr = zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(io::Error::other("failed to construct SCM_RIGHTS header"));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as libc::c_uint) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL)
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent != 1 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "capture descriptor marker was not written atomically",
        ));
    }
    Ok(())
}

fn receive_fd(stream: &UnixStream) -> io::Result<RawFd> {
    let mut marker = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&mut marker as *mut u8).cast(),
        iov_len: 1,
    };
    // SAFETY: CMSG_SPACE computes storage for exactly one descriptor.
    let control_len = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as libc::c_uint) } as usize;
    let mut control = vec![0_u8; control_len];
    // SAFETY: all msghdr pointers target live writable storage.
    let (received, flags, fd) = unsafe {
        let mut message: libc::msghdr = zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        let received = libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        let header = libc::CMSG_FIRSTHDR(&message);
        let fd = if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
            || (*header).cmsg_len < libc::CMSG_LEN(size_of::<RawFd>() as libc::c_uint) as usize
        {
            -1
        } else {
            std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>())
        };
        (received, message.msg_flags, fd)
    };
    if received != 1 || marker != BLOB_MARKER || flags & libc::MSG_CTRUNC != 0 || fd < 0 {
        if fd >= 0 {
            // SAFETY: the received descriptor is rejected before ownership is wrapped.
            unsafe { libc::close(fd) };
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing or truncated capture descriptor",
        ));
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_blob_round_trips() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let blob = SealedBlob::new(b"immutable pixels").unwrap();
        blob.send(&sender).unwrap();
        assert_eq!(receive(&receiver, blob.len()).unwrap(), b"immutable pixels");
    }

    #[test]
    fn blob_length_is_bounded_before_allocation() {
        assert!(validate_len(0).is_err());
        assert!(validate_len(MAX_BLOB_BYTES).is_ok());
        assert!(validate_len(MAX_BLOB_BYTES + 1).is_err());
    }

    /// A memfd without seals stands in for a dmabuf: both are plain
    /// descriptors without seal support on the wire. A memfd still stats as
    /// a regular file; the anonymous-inode file type of a real dmabuf cannot
    /// be emulated here.
    fn unsealed_memfd(bytes: &[u8]) -> File {
        // SAFETY: the name is static and NUL-terminated; the returned fd is
        // checked before ownership is constructed.
        let fd = unsafe { libc::memfd_create(c"tessera-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create: {}", io::Error::last_os_error());
        // SAFETY: `fd` is a new owned descriptor from memfd_create.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes).unwrap();
        file
    }

    #[test]
    fn dmabuf_receive_accepts_an_unsealed_descriptor() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut file = unsealed_memfd(b"gpu plane");
        let len = file.metadata().unwrap().len();
        send_fd(&sender, file.as_raw_fd()).unwrap();
        let mut received = receive_dmabuf_file(&receiver, len).unwrap();
        let mut bytes = Vec::new();
        received.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"gpu plane");
        file.seek(SeekFrom::Start(0)).ok();
    }

    #[test]
    fn dmabuf_receive_accepts_a_larger_allocation() {
        // GPU allocations may exceed the announced plane bytes.
        let (sender, receiver) = UnixStream::pair().unwrap();
        let file = unsealed_memfd(b"gpu plane with slack");
        send_fd(&sender, file.as_raw_fd()).unwrap();
        let mut received = receive_dmabuf_file(&receiver, 9).unwrap();
        let mut bytes = Vec::new();
        received.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"gpu plane with slack");
    }

    #[test]
    fn memfd_receive_rejects_an_unsealed_descriptor() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let file = unsealed_memfd(b"mutable pixels");
        let len = file.metadata().unwrap().len();
        send_fd(&sender, file.as_raw_fd()).unwrap();
        assert!(receive_memfd_file(&receiver, len).is_err());
    }

    #[test]
    fn receive_rejects_a_length_mismatch() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let file = unsealed_memfd(b"short");
        send_fd(&sender, file.as_raw_fd()).unwrap();
        assert!(receive_dmabuf_file(&receiver, 4096).is_err());
    }
}
