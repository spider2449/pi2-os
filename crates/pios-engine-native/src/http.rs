//! Just enough HTTP/1.1 for the Docker Engine API (server side) and for
//! registry downloads (client side). Blocking I/O, no async runtime.

use std::io::{self, BufRead, Read, Write};

const MAX_HEADER_BYTES: usize = 64 << 10;
const MAX_BODY: usize = 16 << 20;

#[derive(Debug, Default)]
pub struct Request {
    pub method: String,
    /// Decoded path without the query and without a `/v1.xx` prefix.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// HTTP/1.0 or `Connection: close`.
    pub close: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Docker's boolean query values: `1`, `true`, `True`.
    pub fn flag(&self, name: &str) -> bool {
        self.query(name).is_some_and(|v| matches!(v, "1" | "true" | "True" | "TRUE"))
    }

    pub fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }
}

pub fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Read the status/request line and headers. `None` on a clean EOF.
fn read_head(r: &mut dyn BufRead) -> io::Result<Option<(String, Vec<(String, String)>)>> {
    let mut first = String::new();
    loop {
        first.clear();
        if r.read_line(&mut first)? == 0 {
            return Ok(None);
        }
        // Tolerate stray CRLFs between keep-alive requests (RFC 9112 2.2).
        if !first.trim().is_empty() {
            break;
        }
    }
    let mut headers = Vec::new();
    let mut total = first.len();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Err(bad("connection closed inside headers"));
        }
        total += line.len();
        if total > MAX_HEADER_BYTES {
            return Err(bad("headers too large"));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Some((first.trim_end().to_string(), headers)))
}

fn read_body(r: &mut dyn BufRead, headers: &[(String, String)]) -> io::Result<Vec<u8>> {
    if find_header(headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        let mut body = Vec::new();
        ChunkedReader::new(r).take(MAX_BODY as u64 + 1).read_to_end(&mut body)?;
        if body.len() > MAX_BODY {
            return Err(bad("request body too large"));
        }
        return Ok(body);
    }
    let len: usize = match find_header(headers, "content-length") {
        Some(v) => v.parse().map_err(|_| bad("bad content-length"))?,
        None => 0,
    };
    if len > MAX_BODY {
        return Err(bad("request body too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

pub fn read_request(r: &mut dyn BufRead) -> io::Result<Option<Request>> {
    let Some((line, headers)) = read_head(r)? else {
        return Ok(None);
    };
    let mut parts = line.split(' ');
    let (method, target, version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v)) => (m, t, v),
        _ => return Err(bad(format!("bad request line: {line}"))),
    };
    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut path = percent_decode(raw_path);
    // Strip the API version prefix: /v1.47/containers/json -> /containers/json.
    if let Some(rest) = path.strip_prefix("/v") {
        if let Some(i) = rest.find('/') {
            if rest[..i].split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
                path = rest[i..].to_string();
            }
        }
    }
    let query = parse_query(raw_query);
    let close = version == "HTTP/1.0"
        || find_header(&headers, "connection").is_some_and(|v| v.eq_ignore_ascii_case("close"));
    let body = read_body(r, &headers)?;
    Ok(Some(Request { method: method.to_string(), path, query, headers, body, close }))
}

pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(&k.replace('+', " ")), percent_decode(&v.replace('+', " ")))
        })
        .collect()
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hexval = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn reason(status: u16) -> &'static str {
    match status {
        101 => "UPGRADED",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Unknown",
    }
}

/// Headers every API response carries (API version negotiation).
pub const API_HEADERS: &str = "Api-Version: 1.47\r\nDocker-Experimental: false\r\nOstype: linux\r\nServer: PiOS-native\r\n";

/// Write a complete response with a body.
pub fn respond(w: &mut dyn Write, status: u16, content_type: &str, body: &[u8], close: bool) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n{API_HEADERS}", reason(status));
    if !content_type.is_empty() {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    if close {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Start a chunked response; write the body through `ChunkedWriter`.
pub fn respond_chunked(w: &mut dyn Write, status: u16, content_type: &str) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\n{API_HEADERS}Content-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n",
        reason(status)
    );
    w.write_all(head.as_bytes())?;
    w.flush()
}

pub struct ChunkedWriter<'a> {
    inner: &'a mut dyn Write,
}

impl<'a> ChunkedWriter<'a> {
    pub fn new(inner: &'a mut dyn Write) -> Self {
        Self { inner }
    }

    /// One chunk, flushed (progress streams must reach the client promptly).
    pub fn chunk(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        write!(self.inner, "{:x}\r\n", data.len())?;
        self.inner.write_all(data)?;
        self.inner.write_all(b"\r\n")?;
        self.inner.flush()
    }

    pub fn finish(self) -> io::Result<()> {
        self.inner.write_all(b"0\r\n\r\n")?;
        self.inner.flush()
    }
}

/// Decodes a chunked body; trailers are discarded.
pub struct ChunkedReader<R> {
    inner: R,
    remaining: u64,
    done: bool,
}

impl<R: BufRead> ChunkedReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, remaining: 0, done: false }
    }
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut line = String::new();
            self.inner.read_line(&mut line)?;
            let size = line.trim().split(';').next().unwrap_or("");
            self.remaining = u64::from_str_radix(size, 16).map_err(|_| bad(format!("bad chunk size {size:?}")))?;
            if self.remaining == 0 {
                // Trailers until an empty line.
                loop {
                    line.clear();
                    if self.inner.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed inside a chunk"));
        }
        self.remaining -= n as u64;
        if self.remaining == 0 {
            let mut crlf = [0u8; 2];
            self.inner.read_exact(&mut crlf)?;
        }
        Ok(n)
    }
}

/// Status and headers of a client response; the body is read separately.
#[derive(Debug)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

pub fn read_response_head(r: &mut dyn BufRead) -> io::Result<ResponseHead> {
    let (line, headers) = read_head(r)?.ok_or_else(|| bad("connection closed before the response"))?;
    let status = line.split(' ').nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| bad(format!("bad status line: {line}")))?;
    Ok(ResponseHead { status, headers })
}

/// Body reader matching the response framing.
pub fn body_reader<'a, R: BufRead + Send + 'a>(head: &ResponseHead, r: R) -> Box<dyn Read + Send + 'a> {
    if head.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        Box::new(ChunkedReader::new(r))
    } else if let Some(len) = head.header("content-length").and_then(|v| v.parse::<u64>().ok()) {
        Box::new(r.take(len))
    } else {
        Box::new(r)
    }
}

/// Docker's stream multiplexing for non-TTY attach/logs: an 8-byte header
/// (stream id, 3 zero bytes, big-endian length) before each frame.
pub fn mux_frame(stream: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data.len());
    v.extend_from_slice(&[stream, 0, 0, 0]);
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn parses_versioned_request_with_body() {
        let raw = b"POST /v1.47/containers/create?name=web%201&x=a+b HTTP/1.1\r\nHost: docker\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}GET /_ping HTTP/1.1\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        let req = read_request(&mut r).unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/containers/create");
        assert_eq!(req.query("name"), Some("web 1"));
        assert_eq!(req.query("x"), Some("a b"));
        assert_eq!(req.body, b"{}");
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert!(!req.close);
        let ping = read_request(&mut r).unwrap().unwrap();
        assert_eq!(ping.path, "/_ping");
        assert!(read_request(&mut r).unwrap().is_none());
    }

    #[test]
    fn keeps_paths_that_only_look_versioned() {
        let raw = b"GET /volumes/v1/x HTTP/1.0\r\n\r\n";
        let req = read_request(&mut BufReader::new(&raw[..])).unwrap().unwrap();
        assert_eq!(req.path, "/volumes/v1/x");
        assert!(req.close);
    }

    #[test]
    fn chunked_round_trip() {
        let mut out = Vec::new();
        let mut w = ChunkedWriter::new(&mut out);
        w.chunk(b"hello ").unwrap();
        w.chunk(b"world").unwrap();
        w.finish().unwrap();
        let mut body = String::new();
        ChunkedReader::new(BufReader::new(&out[..])).read_to_string(&mut body).unwrap();
        assert_eq!(body, "hello world");
    }

    #[test]
    fn client_response_framing() {
        let raw = b"HTTP/1.1 307 Temporary Redirect\r\nLocation: https://cdn/x\r\nContent-Length: 3\r\n\r\nabcrest";
        let mut r = BufReader::new(&raw[..]);
        let head = read_response_head(&mut r).unwrap();
        assert_eq!(head.status, 307);
        assert_eq!(head.header("location"), Some("https://cdn/x"));
        let mut body = String::new();
        body_reader(&head, &mut r).read_to_string(&mut body).unwrap();
        assert_eq!(body, "abc");
    }

    #[test]
    fn percent_coding_and_mux() {
        assert_eq!(percent_decode("a%2Fb%zz%"), "a/b%zz%");
        assert_eq!(percent_encode("repository:library/alpine:pull"), "repository%3Alibrary%2Falpine%3Apull");
        assert_eq!(mux_frame(1, b"hi"), [1, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']);
    }
}
