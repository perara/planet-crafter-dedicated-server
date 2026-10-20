//! An HTTP request a plugin makes (capability `net.http`, ABI 1.2) and the pairs of headers its
//! answer carries. The request is encoded with the codec and handed to `pcds.http_request`; the
//! answer comes later as `Event::HttpDone`. New fields are only added at the end.

use alloc::vec::Vec;

use crate::codec::{DecodeError, Reader, Writer};

/// Most header pairs a request or an answer carries.
pub const MAX_HEADERS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest<'a> {
    /// GET, HEAD, POST, PUT, PATCH or DELETE.
    pub method: &'a str,
    /// `https://host[:port]/path?query`. Where it may go is the manifest's `[net] allow` list. A
    /// piece of it may be `${secret.NAME}`: the host puts the admin's secret there (a webhook
    /// URL, a token) and the plugin never sees it.
    pub url: &'a str,
    /// Header values may hold `${secret.NAME}` as well. `Host`, `Content-Length` and the like are
    /// the host's and refused.
    pub headers: Vec<(&'a str, &'a str)>,
    pub body: &'a [u8],
}

impl<'a> HttpRequest<'a> {
    pub fn encode(&self, w: &mut Writer) {
        w.str(self.method).str(self.url);
        encode_pairs(&self.headers, w);
        w.bytes(self.body);
    }

    pub fn decode(bytes: &'a [u8]) -> Result<HttpRequest<'a>, DecodeError> {
        let mut r = Reader::new(bytes);
        Ok(HttpRequest { method: r.str()?, url: r.str()?, headers: decode_pairs(&mut r)?, body: r.bytes()? })
    }
}

pub fn encode_pairs(pairs: &[(&str, &str)], w: &mut Writer) {
    w.u32(pairs.len() as u32);
    for (k, v) in pairs {
        w.str(k).str(v);
    }
}

pub fn decode_pairs<'a>(r: &mut Reader<'a>) -> Result<Vec<(&'a str, &'a str)>, DecodeError> {
    let n = r.u32()? as usize;
    // Each pair takes at least 8 bytes: a count the bytes cannot hold is short (and allocates nothing).
    if n > r.rest().len() / 8 {
        return Err(DecodeError::Short);
    }
    let mut pairs = Vec::with_capacity(n);
    for _ in 0..n {
        pairs.push((r.str()?, r.str()?));
    }
    Ok(pairs)
}
