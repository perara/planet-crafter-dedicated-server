//! The boundary between the front and a bank (docs/hot-update.md): one unix stream socket per
//! bank process, carrying frames `u32 length of the rest, u8 kind, u64 n, body`. A snapshot
//! never crosses the socket: it lives in a sealed memfd whose descriptor travels with its frame
//! (SCM_RIGHTS), from the old bank to the front and on to the new one.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The frames below. A bank that says another one in its HELLO is not used: such an update
/// needs the front restarted (`pcds restart`). 3: HANDOFF and SNAPSHOT carry the flip's number.
pub const PROTOCOL: u64 = 3;

// game -> front: n PROTOCOL; body JSON {"version", "gameVersion", "format", "reads"}.
pub const HELLO: u8 = 1;
// front -> bank: load the save.
pub const START: u8 = 2;
// front -> bank: a memfd with a snapshot; body JSON {"recover": bool} (recover: every client
// in it is gone, the bank restarts after a crash).
pub const RESTORE: u8 = 3;
// bank -> front: running (the health check); body JSON {"restoreMs", "verified"}.
pub const READY: u8 = 4;
// front -> bank: n client; body JSON {"ip", "steamId", "trusted"}.
pub const CONNECTED: u8 = 5;
// both ways: n client; body one Netcode batch.
pub const DATA: u8 = 6;
// front -> bank: n client left.
pub const GONE: u8 = 7;
// bank -> front: n client kicked: its reason went out as DATA, close it a moment later.
pub const KICK: u8 = 8;
// front -> bank: n the flip; send everything queued, stop, snapshot.
pub const HANDOFF: u8 = 9;
// bank -> front: n the flip of the HANDOFF it answers; a memfd with the snapshot, body JSON
// {"bytes", "ms"}; no memfd: none could be written and the bank went on. Then it waits for EXIT
// or RESUME. The front takes only the SNAPSHOT of the flip under way.
pub const SNAPSHOT: u8 = 10;
// front -> old bank: the new one runs, end.
pub const EXIT: u8 = 11;
// front -> old bank: the new one failed, go on.
pub const RESUME: u8 = 12;
// front -> bank: n request id; body JSON {"method", "path", "body"}.
pub const ADMIN: u8 = 13;
// bank -> front: n request id; body JSON {"status", "body"}.
pub const ANSWER: u8 = 14;
// front -> bank: the autosave is due.
pub const SAVE: u8 = 15;
// front -> bank: save and end.
pub const STOP: u8 = 16;
// bank -> front: a chat event (chathooks.rs) for the feed.
pub const CHAT: u8 = 17;
// front -> bank: body text into the chat, from the server.
pub const SAY: u8 = 18;
// bank -> front: body JSON, the rules' chat section (the hooks' settings).
pub const CHATCFG: u8 = 19;
// bank -> front: body JSON {"autosaveMinutes", "autoUpdate", "updateCheckHours"}, the rules the
// front acts on (save, update); at the start and whenever they change.
pub const OPS: u8 = 20;

/// The largest frame either side takes (a world's join batch is a few MiB; a snapshot never
/// crosses the socket). A frame shorter than its header or longer than this breaks the link.
pub const MAX_FRAME: usize = 64 << 20;

/// Whether a frame with this body is within MAX_FRAME.
pub fn fits(body: &[u8]) -> bool {
    9 + body.len() <= MAX_FRAME
}

/// The fds a frame of this kind carries.
fn carries_fd(kind: u8, body: &[u8]) -> bool {
    kind == RESTORE || (kind == SNAPSHOT && !body.is_empty())
}

pub struct Frame {
    pub kind: u8,
    pub n: u64,
    pub body: Vec<u8>,
    pub fd: Option<OwnedFd>,
}

/// Bytes to write: one frame (or what is left of it) and the descriptor it carries.
struct Out {
    bytes: Vec<u8>,
    at: usize,
    fd: Option<OwnedFd>,
    /// The client a client frame came from (its bytes count against its bound), else 0.
    client: u64,
}

pub fn encode(kind: u8, n: u64, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(13 + body.len());
    f.extend_from_slice(&(9 + body.len() as u32).to_le_bytes());
    f.push(kind);
    f.extend_from_slice(&n.to_le_bytes());
    f.extend_from_slice(body);
    f
}

/// One end of the socket. The front's is non-blocking with a queue (it never waits for a bank);
/// a bank's blocks on writes (a front that stopped reading stalls the world, which loses
/// nothing).
pub struct Link {
    pub stream: UnixStream,
    inbox: Vec<u8>,
    /// How much of the inbox's start next() has taken (dropped in one move before the next fill,
    /// not after every frame).
    taken: usize,
    /// fill's read buffer, made on first use and kept.
    scratch: Vec<u8>,
    fds: VecDeque<OwnedFd>,
    outbox: VecDeque<Out>,
    /// Bytes queued per client (front only): the caller drops a client over its bound.
    pub queued: crate::idmap::FxHashMap<u64, usize>,
    blocking: bool,
    /// Why the peer's bytes are no frames (a length out of bounds): the link reads nothing
    /// more and counts as gone.
    pub broken: Option<String>,
}

impl Link {
    pub fn new(stream: UnixStream) -> Link {
        // SAFETY: F_GETFL only reads the descriptor's flags.
        let blocking = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) & libc::O_NONBLOCK == 0 };
        Link { stream, inbox: Vec::new(), taken: 0, scratch: Vec::new(), fds: VecDeque::new(), outbox: VecDeque::new(), queued: Default::default(), blocking, broken: None }
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Queues a frame (with its memfd); written by flush(). One over MAX_FRAME is not sent (the
    /// other side would break the link on it), with a line in the log.
    pub fn queue(&mut self, kind: u8, n: u64, body: &[u8], fd: Option<OwnedFd>, client: u64) {
        if !fits(body) {
            eprintln!("ipc: a frame {kind} of {} bytes not sent: a frame holds at most {MAX_FRAME}", 9 + body.len());
            return;
        }
        let bytes = encode(kind, n, body);
        if client != 0 {
            *self.queued.entry(client).or_default() += bytes.len();
        }
        self.outbox.push_back(Out { bytes, at: 0, fd, client });
    }

    pub fn pending(&self) -> bool {
        !self.outbox.is_empty()
    }

    /// Writes what the socket takes now; Err once the peer is gone. Frames queued together go in
    /// one system call (a frame with a descriptor goes alone).
    pub fn flush(&mut self) -> std::io::Result<()> {
        while let Some(o) = self.outbox.front_mut() {
            let n = match o.fd.take() {
                Some(fd) => match send_with_fd(self.stream.as_raw_fd(), &o.bytes[o.at..], fd.as_raw_fd()) {
                    Ok(n) => n,
                    Err(e) => {
                        o.fd = Some(fd);
                        return if e.kind() == ErrorKind::WouldBlock { Ok(()) } else { Err(e) };
                    }
                },
                None => match self.write_queued() {
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                },
            };
            self.written(n);
        }
        Ok(())
    }

    /// One writev of the frames at the front of the queue that carry no descriptor.
    fn write_queued(&self) -> std::io::Result<usize> {
        const FRAMES: usize = 64;
        let mut iov = [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; FRAMES];
        let mut count = 0;
        for (slot, o) in iov.iter_mut().zip(self.outbox.iter().take_while(|o| o.fd.is_none())) {
            *slot = libc::iovec { iov_base: o.bytes[o.at..].as_ptr() as *mut libc::c_void, iov_len: o.bytes.len() - o.at };
            count += 1;
        }
        // SAFETY: the first `count` iovecs point into frames of the queue, which stays untouched
        // until the call returns; writev only reads them.
        let n = unsafe { libc::writev(self.stream.as_raw_fd(), iov.as_ptr(), count as libc::c_int) };
        if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
    }

    /// `n` bytes of the queue went out: the frames they complete leave it.
    fn written(&mut self, mut n: usize) {
        while n > 0 {
            let Some(o) = self.outbox.front_mut() else { return };
            let left = o.bytes.len() - o.at;
            if n < left {
                o.at += n;
                return;
            }
            n -= left;
            let o = self.outbox.pop_front().unwrap();
            if o.client != 0
                && let Some(q) = self.queued.get_mut(&o.client)
            {
                *q = q.saturating_sub(o.bytes.len());
                if *q == 0 {
                    self.queued.remove(&o.client);
                }
            }
        }
    }

    /// A frame written at once, waiting for the socket (a bank's writes).
    pub fn send(&mut self, kind: u8, n: u64, body: &[u8]) -> bool {
        self.send_fd(kind, n, body, None)
    }

    pub fn send_fd(&mut self, kind: u8, n: u64, body: &[u8], fd: Option<OwnedFd>) -> bool {
        self.queue(kind, n, body, fd, 0);
        self.drain()
    }

    /// Writes everything queued, waiting for the socket; false once the peer is gone.
    pub fn drain(&mut self) -> bool {
        loop {
            match self.flush() {
                Ok(()) if !self.pending() => return true,
                Ok(()) => wait_out(self.fd()),
                Err(_) => return false,
            }
        }
    }

    /// Reads what is waiting, at most `max` bytes; false once the peer is gone. A blocking
    /// socket blocks here until something comes.
    pub fn fill(&mut self, max: usize) -> bool {
        if self.broken.is_some() {
            return false;
        }
        self.inbox.drain(..std::mem::take(&mut self.taken));
        let mut buf = std::mem::take(&mut self.scratch);
        buf.resize(1 << 16, 0);
        let mut read = 0;
        let alive = loop {
            if read >= max {
                break true;
            }
            match recv_with_fds(self.stream.as_raw_fd(), &mut buf, &mut self.fds) {
                Ok(0) => break false,
                Ok(n) => {
                    self.inbox.extend_from_slice(&buf[..n]);
                    read += n;
                    // A blocking socket would wait for more: what came is enough. So is a read
                    // that did not fill the buffer: the socket is empty now (what comes after is
                    // read in the next loop, the descriptor stays readable).
                    if self.blocking || n < buf.len() {
                        break true;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break true,
                Err(_) => break false,
            }
        };
        self.scratch = buf;
        alive
    }

    /// The next whole frame read, if any. A length out of bounds breaks the link (`broken`):
    /// None from here on, and fill() says the peer is gone.
    pub fn next(&mut self) -> Option<Frame> {
        if self.broken.is_some() {
            return None;
        }
        let inbox = &self.inbox[self.taken..];
        let n = u32::from_le_bytes(inbox.get(..4)?.try_into().ok()?) as usize;
        if !(9..=MAX_FRAME).contains(&n) {
            self.broken = Some(format!("a frame of {n} bytes (a frame is 9 to {MAX_FRAME})"));
            self.inbox.clear();
            self.taken = 0;
            return None;
        }
        let f = inbox.get(4..4 + n)?;
        let (kind, num, body) = (f[0], u64::from_le_bytes(f[1..9].try_into().ok()?), f[9..].to_vec());
        self.taken += 4 + n;
        let fd = if carries_fd(kind, &body) { self.fds.pop_front() } else { None };
        Some(Frame { kind, n: num, body, fd })
    }

    /// Waits for one frame (a blocking socket); None once the peer is gone.
    pub fn recv(&mut self) -> Option<Frame> {
        loop {
            if let Some(f) = self.next() {
                return Some(f);
            }
            if !self.fill(usize::MAX) {
                return None;
            }
        }
    }
}

/// Waits until the descriptor can be written.
fn wait_out(fd: RawFd) {
    let mut p = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
    // SAFETY: one valid pollfd for the call's duration.
    unsafe { libc::poll(&mut p, 1, 1000) };
}

/// Waits until a descriptor is ready or `timeout` passes; which ones are. `out[i]`: wait for
/// writable too.
pub fn wait(fds: &[RawFd], out: &[bool], timeout: Duration) -> Vec<bool> {
    let mut p: Vec<libc::pollfd> = fds.iter().zip(out.iter().chain(std::iter::repeat(&false))).map(|(fd, o)| libc::pollfd { fd: *fd, events: libc::POLLIN | if *o { libc::POLLOUT } else { 0 }, revents: 0 }).collect();
    let ts = libc::timespec { tv_sec: timeout.as_secs() as libc::time_t, tv_nsec: timeout.subsec_nanos() as libc::c_long };
    // SAFETY: valid pollfds and timespec for the call's duration; no signal mask.
    unsafe { libc::ppoll(p.as_mut_ptr(), p.len() as libc::nfds_t, &ts, std::ptr::null()) };
    p.iter().map(|p| p.revents != 0).collect()
}

const CMSG_FDS: usize = 4;

fn send_with_fd(sock: RawFd, data: &[u8], fd: RawFd) -> std::io::Result<usize> {
    // SAFETY: msghdr points at a live iovec and an 8-aligned control buffer of at least CMSG_SPACE
    // for one fd; CMSG_FIRSTHDR of that buffer is non-null and in bounds.
    unsafe {
        let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        // u64s, not bytes: the control message headers read from it need 8-byte alignment.
        let mut control = vec![0u64; space.div_ceil(8)];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd);
        let n = libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL);
        if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
    }
}

fn recv_with_fds(sock: RawFd, buf: &mut [u8], fds: &mut VecDeque<OwnedFd>) -> std::io::Result<usize> {
    // SAFETY: as in send_with_fd; every fd the kernel put in the control buffer is taken into an
    // OwnedFd exactly once.
    unsafe {
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
        let space = libc::CMSG_SPACE((CMSG_FDS * std::mem::size_of::<RawFd>()) as u32) as usize;
        // u64s: aligned for a cmsghdr; room for CMSG_FDS descriptors (checked below).
        let mut control = [0u64; 8];
        assert!(space <= std::mem::size_of_val(&control));
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space;
        let n = libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..count {
                    fds.push_back(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        Ok(n as usize)
    }
}

/// A sealed memfd holding what `write` writes: nobody can change it once it is passed on.
pub fn memfd(name: &str, write: impl FnOnce(&mut std::io::BufWriter<&File>) -> Result<(), String>) -> Result<OwnedFd, String> {
    let name = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
    // SAFETY: a valid C string; the fd returned is new and owned here.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(format!("memfd: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fd was just created and is owned by nobody else.
    let file = unsafe { File::from_raw_fd(fd) };
    {
        let mut w = std::io::BufWriter::with_capacity(1 << 20, &file);
        write(&mut w)?;
        w.flush().map_err(|e| format!("memfd: {e}"))?;
    }
    // SAFETY: fcntl on our own fd.
    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL) };
    Ok(file.into())
}

/// A memfd's whole content (read at offset 0: the fd's offset is shared with every holder).
pub fn read_memfd(fd: &OwnedFd) -> std::io::Result<Vec<u8>> {
    let file = File::from(fd.try_clone()?);
    let len = file.metadata()?.len() as usize;
    let mut buf = vec![0u8; len];
    file.read_exact_at(&mut buf, 0)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn frames_and_memfds_cross_the_socket() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let (mut front, mut bank) = (Link::new(a), Link::new(b));
        let fd = memfd("test", |w| w.write_all(b"the world").map_err(|e| e.to_string())).unwrap();
        // Sealed: no more writes.
        assert!(File::from(fd.try_clone().unwrap()).write_all(b"x").is_err());
        front.queue(DATA, 7, b"batch", None, 7);
        front.queue(RESTORE, 0, b"{}", Some(fd), 0);
        front.queue(GONE, 7, &[], None, 0);
        assert_eq!(front.queued[&7], 13 + 5);
        front.flush().unwrap();
        assert!(front.queued.is_empty() && !front.pending());
        let f = bank.recv().unwrap();
        assert_eq!((f.kind, f.n, f.body.as_slice(), f.fd.is_none()), (DATA, 7, &b"batch"[..], true));
        let f = bank.recv().unwrap();
        assert_eq!((f.kind, read_memfd(f.fd.as_ref().unwrap()).unwrap()), (RESTORE, b"the world".to_vec()));
        assert_eq!(bank.recv().unwrap().kind, GONE);
        // A front that never blocks: past what the socket holds, frames wait in the queue.
        let big = vec![0u8; 1 << 20];
        for _ in 0..16 {
            front.queue(DATA, 3, &big, None, 3);
            front.flush().unwrap();
        }
        assert!(front.pending() && front.queued[&3] > 0);
        drop(bank);
        assert!(front.flush().is_err(), "the peer is gone");
    }

    /// Many frames queued together cross in order and whole (writev stops mid-frame when the
    /// socket is full), with a descriptor-carrying one among them going alone; the per-client
    /// count falls to zero.
    #[test]
    fn frames_queued_together_arrive_in_order() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let (mut front, mut bank) = (Link::new(a), Link::new(b));
        let reader = std::thread::spawn(move || (0..600u64).map(|_| bank.recv().map(|f| (f.kind, f.n, f.body.len(), f.fd.is_some())).unwrap()).collect::<Vec<_>>());
        for i in 0..600u64 {
            let body = vec![i as u8; 1 + (i as usize * 37) % 9000];
            if i == 300 {
                let fd = memfd("test", |w| w.write_all(b"x").map_err(|e| e.to_string())).unwrap();
                front.queue(RESTORE, i, &body, Some(fd), 0);
            } else {
                front.queue(DATA, i, &body, None, 5);
            }
        }
        assert!(front.drain());
        let got = reader.join().unwrap();
        assert!(front.queued.is_empty() && !front.pending());
        for (i, (kind, n, len, fd)) in got.into_iter().enumerate() {
            assert_eq!((kind, n, len, fd), (if i == 300 { RESTORE } else { DATA }, i as u64, 1 + (i * 37) % 9000, i == 300));
        }
    }

    #[test]
    fn a_frame_out_of_bounds_breaks_the_link_and_never_panics() {
        for n in [0u32, 8, (MAX_FRAME + 1) as u32, u32::MAX] {
            let (a, b) = UnixStream::pair().unwrap();
            a.set_nonblocking(true).unwrap();
            let mut front = Link::new(a);
            let mut bank = Link::new(b);
            bank.stream.write_all(&n.to_le_bytes()).unwrap();
            bank.stream.write_all(&[DATA; 8]).unwrap();
            assert!(front.fill(usize::MAX));
            assert!(front.next().is_none(), "{n}");
            assert!(front.broken.as_ref().is_some_and(|b| b.contains(&format!("{n} bytes"))), "{n}");
            // From here the link counts as gone, even with more bytes coming.
            bank.send(DATA, 1, b"x");
            assert!(!front.fill(usize::MAX) && front.next().is_none());
        }
    }

    /// A hostile client's datagrams hold hundreds of 1-byte batches each; the bank reads them
    /// all at once, so taking a frame off the inbox must not move everything behind it.
    #[test]
    fn a_flood_of_tiny_frames_is_read_in_linear_time() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut link = Link::new(a);
        let frames = 100_000;
        for _ in 0..frames {
            link.inbox.extend_from_slice(&encode(DATA, 1, b"x"));
        }
        let start = Instant::now();
        let mut read = 0;
        while link.next().is_some() {
            read += 1;
        }
        assert_eq!(read, frames);
        assert!(start.elapsed() < Duration::from_secs(2), "{frames} frames took {:?}", start.elapsed());
    }

    /// The socket's cost alone, between two processes (docs/hot-update.md, "Latency budget"):
    ///   cargo test --release ipc::tests::ipc -- --ignored --nocapture
    #[test]
    #[ignore]
    fn ipc_echo() {
        let Some(path) = std::env::var_os("PCDS_IPC_ECHO") else { return };
        let mut l = Link::new(UnixStream::connect(path).unwrap());
        loop {
            if !wait(&[l.fd()], &[], Duration::from_millis(5))[0] {
                continue;
            }
            if !l.fill(usize::MAX) {
                break;
            }
            while let Some(f) = l.next() {
                if f.kind == EXIT {
                    return;
                }
                l.send_fd(f.kind, f.n, &f.body, f.fd);
            }
        }
    }

    #[test]
    #[ignore]
    fn ipc() {
        let path = std::env::temp_dir().join(format!("pcds-ipc-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap()).args(["ipc::tests::ipc_echo", "--ignored", "--exact", "--nocapture"]).env("PCDS_IPC_ECHO", &path).stdout(std::process::Stdio::null()).spawn().unwrap();
        let mut l = Link::new(listener.accept().unwrap().0);
        std::fs::remove_file(&path).unwrap();
        let body = vec![7u8; 120];
        let mut rounds: Vec<Duration> = (0..20_000)
            .map(|_| {
                let t = Instant::now();
                l.send(DATA, 1, &body);
                l.recv().unwrap();
                t.elapsed()
            })
            .collect();
        rounds.sort();
        // A snapshot the size of the live world's: its memfd passed there and back.
        let snapshot = vec![b'x'; 4 << 20];
        let t = Instant::now();
        let fd = memfd("snapshot", |w| w.write_all(&snapshot).map_err(|e| e.to_string())).unwrap();
        l.send_fd(SNAPSHOT, 0, b"{}", Some(fd));
        let back = l.recv().unwrap();
        assert!(back.fd.is_some());
        let read = read_memfd(back.fd.as_ref().unwrap()).unwrap();
        let bulk = t.elapsed();
        assert_eq!(read.len(), snapshot.len());
        l.send(EXIT, 0, &[]);
        child.wait().unwrap();
        println!("ipc: 20000 round trips of a 120-byte frame: p50 {:?}, p99 {:?}; a 4 MiB snapshot written, passed there and back as a memfd and read: {bulk:?}", rounds[10_000], rounds[19_800]);
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/targets/ipc.rs"]
pub mod fuzz;
