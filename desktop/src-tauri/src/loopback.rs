//! A minimal HTTP/1.1 client for talking to the project daemons.
//!
//! Every call this host makes goes to `127.0.0.1:<port>` over plain HTTP with a
//! small JSON body: `/hello`, `/manager-heartbeat`, `/manager-close`, `/stop`,
//! `/resolve`. That is the entire requirement, and meeting it directly is worth
//! doing rather than pulling a general HTTP stack in behind it:
//!
//! * A general client brings a TLS stack that this code can never use. Sharing
//!   one with the updater also means sharing its feature flags — the first
//!   version of this module used `reqwest`, and the unified `rustls` features
//!   made `Client::builder().build()` panic for want of a crypto provider that
//!   nothing on a loopback path should ever need.
//! * No connection pool means no chance of reusing a socket to a daemon that
//!   has since exited on the same scanned port.
//!
//! Deliberately absent: TLS, redirects, cookies, keep-alive, compression, and
//! any header the daemon did not ask for. In particular there is **no `Origin`
//! header** — a native loopback client carries none, which is what puts these
//! requests on the daemon's un-gated path rather than its browser-origin one
//! (Design 5.2).

use std::time::Duration;

use serde_json::Value;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
    time,
};

/// A daemon's answer is a status line and a small JSON document. Anything much
/// larger is a daemon this host should not be reading, so the read is capped.
const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_HEAD: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

impl Response {
    pub(crate) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub(crate) fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

pub(crate) async fn get(port: u16, path: &str, timeout: Duration) -> Result<Response, String> {
    request(port, "GET", path, None, timeout).await
}

pub(crate) async fn post_json(
    port: u16,
    path: &str,
    body: &Value,
    timeout: Duration,
) -> Result<Response, String> {
    let encoded = serde_json::to_vec(body).map_err(|error| error.to_string())?;
    request(port, "POST", path, Some(encoded), timeout).await
}

async fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Response, String> {
    time::timeout(timeout, exchange(port, method, path, body))
        .await
        .map_err(|_| format!("timed out after {}ms", timeout.as_millis()))?
}

async fn exchange(
    port: u16,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<Response, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|error| format!("could not reach 127.0.0.1:{port}: {error}"))?;
    stream.set_nodelay(true).ok();

    let mut head = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         User-Agent: WSync-Desktop/{version}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n",
        version = env!("CARGO_PKG_VERSION"),
    );
    if let Some(body) = &body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|error| format!("could not send the request: {error}"))?;
    if let Some(body) = &body {
        stream
            .write_all(body)
            .await
            .map_err(|error| format!("could not send the body: {error}"))?;
    }
    stream.flush().await.ok();

    // `Connection: close` means the server hangs up when it is done, so reading
    // to EOF is both correct and the simplest framing there is.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("could not read the response: {error}"))?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.len() > MAX_HEAD + MAX_BODY {
            return Err("the daemon's response was too large to read".to_string());
        }
    }

    parse_response(&raw)
}

/// Split a raw response into its status and body, honouring both framings a
/// daemon may pick.
fn parse_response(raw: &[u8]) -> Result<Response, String> {
    let separator = find(raw, b"\r\n\r\n")
        .ok_or_else(|| "the daemon's response had no header terminator".to_string())?;
    if separator > MAX_HEAD {
        return Err("the daemon's response headers were too large".to_string());
    }

    let head = String::from_utf8_lossy(&raw[..separator]);
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| "the daemon's response had no status line".to_string())?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("unreadable status line: {status_line:?}"))?;

    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            content_length = value.parse::<usize>().ok();
        } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }

    let rest = &raw[separator + 4..];
    // 204 and 304 carry no body whatever the headers claim.
    let body = if status == 204 || status == 304 {
        Vec::new()
    } else if chunked {
        decode_chunked(rest)?
    } else if let Some(length) = content_length {
        let end = length.min(rest.len());
        rest[..end].to_vec()
    } else {
        rest.to_vec()
    };

    if body.len() > MAX_BODY {
        return Err("the daemon's response body was too large".to_string());
    }
    Ok(Response { status, body })
}

fn decode_chunked(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line_end = find(rest, b"\r\n").ok_or_else(|| "truncated chunk header".to_string())?;
        let header = String::from_utf8_lossy(&rest[..line_end]);
        // A chunk header may carry extensions after a `;`; the size is the head.
        let size_text = header.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("unreadable chunk size {size_text:?}"))?;
        rest = &rest[line_end + 2..];

        if size == 0 {
            break;
        }
        if rest.len() < size {
            return Err("truncated chunk body".to_string());
        }
        body.extend_from_slice(&rest[..size]);
        if body.len() > MAX_BODY {
            return Err("the daemon's response body was too large".to_string());
        }
        // Each chunk is followed by its own CRLF.
        rest = rest.get(size + 2..).unwrap_or(&[]);
    }
    Ok(body)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"total\":0}\r\nTRAILING";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert!(response.is_success());
        // Exactly Content-Length bytes: nothing after the declared body leaks in.
        assert_eq!(response.body.len(), 13);
        assert_eq!(response.json().unwrap()["total"], 0);
    }

    #[test]
    fn parses_a_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1,\r\n6\r\n\"b\":2}\r\n0\r\n\r\n";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.text(), "{\"a\":1,\"b\":2}");
        assert_eq!(response.json().unwrap()["b"], 2);
    }

    #[test]
    fn parses_a_chunked_response_with_extensions() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;name=value\r\nabc\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap().text(), "abc");
    }

    #[test]
    fn a_204_has_no_body_whatever_the_headers_say() {
        let raw = b"HTTP/1.1 204 No Content\r\nContent-Length: 9\r\n\r\nleftovers";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 204);
        assert!(response.is_success());
        assert!(response.body.is_empty());
    }

    #[test]
    fn a_body_with_no_framing_runs_to_the_end_of_the_stream() {
        let raw = b"HTTP/1.1 500 Internal Server Error\r\nX: y\r\n\r\nboom";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 500);
        assert!(!response.is_success());
        assert_eq!(response.text(), "boom");
        assert!(response.json().is_none());
    }

    #[test]
    fn refuses_responses_it_cannot_frame() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n").is_err());
        assert!(parse_response(b"not http at all\r\n\r\n").is_err());
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err()
        );
    }

    #[tokio::test]
    async fn a_closed_port_is_an_error_not_a_hang() {
        // Bind and drop, so the port is almost certainly free and unowned.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let error = get(port, "/hello", Duration::from_millis(500))
            .await
            .unwrap_err();
        assert!(error.contains("could not reach"), "{error}");
    }
}
