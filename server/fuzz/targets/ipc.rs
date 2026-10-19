//! The front/bank frame reader: bytes in, frames out, never a panic, and a broken link stays broken.
use super::Link;
use std::os::unix::net::UnixStream;

pub fn run(data: &[u8]) {
    let (a, _b) = UnixStream::pair().expect("socketpair");
    let mut link = Link::new(a);
    // The bytes arrive in pieces of a size the input chooses.
    let step = 1 + data.first().map_or(0, |b| *b as usize) * 3;
    for piece in data.chunks(step) {
        link.inbox.extend_from_slice(piece);
        while let Some(f) = link.next() {
            assert!(super::fits(&f.body));
        }
    }
    if link.broken.is_some() {
        assert!(link.next().is_none());
    }
}
