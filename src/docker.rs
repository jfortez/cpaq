//! Minimal Docker Engine API client over the Unix socket.
//!
//! Root-owned auth files are read with `GET .../archive` instead of spawning
//! `docker exec`. The Docker CLI child process dominated the ping's memory peak
//! (~36 MiB versus ~6 MiB for cpaq itself), so the request and the `tar` stream
//! are parsed here with `std` only.

use std::{
    env,
    ffi::OsStr,
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

/// Default Docker Engine socket; `CPA_DOCKER_SOCKET` overrides it.
const DEFAULT_SOCKET: &str = "/var/run/docker.sock";
/// Docker Engine API version the reference client uses.
const API_VERSION: &str = "v1.41";
/// Hard cap on the HTTP response we are willing to buffer.
const MAX_RESPONSE: usize = 1024 * 1024;
/// Socket read/write timeout, so a wedged daemon cannot stall the ping.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
/// One `tar` block; entry contents are padded to it.
const TAR_BLOCK: usize = 512;

/// Tar header offsets.
const TAR_SIZE: std::ops::Range<usize> = 124..136;
const TAR_TYPE: usize = 156;
/// Regular-file type flag (`'\0'` is the historical form).
const TAR_REGULAR: [u8; 2] = [b'0', 0];

const MALFORMED: &str = "Malformed Docker response";
const TRUNCATED: &str = "Truncated Docker response";

/// Docker socket path, honouring `CPA_DOCKER_SOCKET`.
pub fn socket_path() -> PathBuf {
    socket_path_from(env::var_os("CPA_DOCKER_SOCKET").as_deref())
}

fn socket_path_from(value: Option<&OsStr>) -> PathBuf {
    value
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_SOCKET), PathBuf::from)
}

/// Reads one regular file from a container as UTF-8 text.
pub fn read_file(socket: &Path, container: &str, path: &str) -> Result<String, String> {
    let request = format!(
        "GET /{API_VERSION}/containers/{container}/archive?path={} HTTP/1.1\r\n\
         Host: docker\r\n\
         Connection: close\r\n\
         \r\n",
        percent_encode(path)
    );
    let response = round_trip(socket, request.as_bytes()).map_err(str::to_string)?;
    let body = http_body(&response).map_err(str::to_string)?;
    let content = tar_file(&body).map_err(str::to_string)?;
    String::from_utf8(content.to_vec()).map_err(|_| "Auth file is not UTF-8".to_string())
}

/// Sends one request and buffers the whole answer, bounded to [`MAX_RESPONSE`].
fn round_trip(socket: &Path, request: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut stream = UnixStream::connect(socket).map_err(|_| "Cannot reach the Docker socket")?;
    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(SOCKET_TIMEOUT)))
        .map_err(|_| "Cannot reach the Docker socket")?;
    stream
        .write_all(request)
        .and_then(|()| stream.flush())
        .map_err(|_| "Cannot reach the Docker socket")?;
    // GET carries no body; closing our write side ends the request cleanly.
    let _ = stream.shutdown(Shutdown::Write);

    let mut response = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(response),
            Ok(count) => {
                response.extend_from_slice(&chunk[..count]);
                if response.len() > MAX_RESPONSE {
                    return Err("Docker response too large");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("Cannot read the Docker response"),
        }
    }
}

/// Splits an HTTP/1.1 response, honouring `Content-Length` and chunked framing.
fn http_body(response: &[u8]) -> Result<Vec<u8>, &'static str> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(MALFORMED)?;
    let head = std::str::from_utf8(&response[..split]).map_err(|_| MALFORMED)?;
    let mut lines = head.split("\r\n");
    let status = lines.next().ok_or(MALFORMED)?;
    let code = status
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| status.strip_prefix("HTTP/1.0 "))
        .and_then(|rest| rest.split_once(' '))
        .and_then(|(code, _)| code.parse::<u16>().ok())
        .ok_or(MALFORMED)?;
    if !(200..300).contains(&code) {
        // Never echo the daemon's body: it can quote container paths.
        return Err(match code {
            401 | 403 => "Docker denied the container read",
            404 => "Auth file not found in the container",
            _ => "Docker API request failed",
        });
    }

    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value
                .to_ascii_lowercase()
                .split(',')
                .any(|token| token.trim() == "chunked");
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        }
    }

    let body = &response[split + 4..];
    if chunked {
        return dechunk(body);
    }
    match length {
        Some(length) => body.get(..length).map(<[u8]>::to_vec).ok_or(TRUNCATED),
        None => Ok(body.to_vec()),
    }
}

/// Decodes a `Transfer-Encoding: chunked` body.
fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut decoded = Vec::new();
    loop {
        let end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or(MALFORMED)?;
        let header = std::str::from_utf8(&body[..end]).map_err(|_| MALFORMED)?;
        // Chunk extensions after ';' are optional and can be ignored.
        let size = usize::from_str_radix(header.split(';').next().unwrap_or_default().trim(), 16)
            .map_err(|_| MALFORMED)?;
        body = &body[end + 2..];
        if size == 0 {
            return Ok(decoded);
        }
        let chunk = body.get(..size).ok_or(TRUNCATED)?;
        decoded.extend_from_slice(chunk);
        if decoded.len() > MAX_RESPONSE {
            return Err("Docker response too large");
        }
        body = body.get(size..).ok_or(TRUNCATED)?;
        body = body.strip_prefix(b"\r\n").unwrap_or(body);
    }
}

/// Extracts the first regular file entry from a `tar` stream.
fn tar_file(tar: &[u8]) -> Result<&[u8], &'static str> {
    let mut offset = 0;
    while let Some(header) = tar.get(offset..offset + TAR_BLOCK) {
        // Two zero blocks terminate the archive; one is enough to stop here.
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = tar_size(&header[TAR_SIZE])?;
        let start = offset + TAR_BLOCK;
        let end = start.checked_add(size).ok_or(MALFORMED)?;
        let content = tar.get(start..end).ok_or("Truncated container archive")?;
        if TAR_REGULAR.contains(&header[TAR_TYPE]) {
            return Ok(content);
        }
        // Skip directories, PAX headers and other metadata entries.
        offset = end.div_ceil(TAR_BLOCK) * TAR_BLOCK;
    }
    Err("Auth file missing from the container archive")
}

/// Parses the octal size field of a `tar` header.
fn tar_size(field: &[u8]) -> Result<usize, &'static str> {
    let mut size = 0_usize;
    let mut digits = 0_usize;
    for byte in field {
        match byte {
            b'0'..=b'7' => {
                size = size
                    .checked_mul(8)
                    .and_then(|size| size.checked_add(usize::from(byte - b'0')))
                    .ok_or(MALFORMED)?;
                digits += 1;
            }
            0 | b' ' => break,
            _ => return Err(MALFORMED),
        }
    }
    if digits == 0 {
        return Err(MALFORMED);
    }
    Ok(size)
}

/// Percent-encodes a query value, keeping only RFC 3986 unreserved bytes.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
pub(crate) mod testing {
    use std::{
        fs,
        io::{Read, Write},
        os::unix::net::UnixListener,
        path::Path,
        thread::{self, JoinHandle},
    };

    /// Serves `responses` one per connection on a fresh socket in `dir` and
    /// returns the socket path plus the captured request bytes.
    pub(crate) fn serve(
        dir: &Path,
        responses: Vec<Vec<u8>>,
    ) -> (std::path::PathBuf, JoinHandle<Vec<Vec<u8>>>) {
        let socket = dir.join("docker.sock");
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = thread::spawn(move || {
            let mut seen = Vec::new();
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut request = Vec::new();
                let mut chunk = [0_u8; 512];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => request.extend_from_slice(&chunk[..count]),
                    }
                }
                stream.write_all(&response).unwrap();
                let _ = stream.flush();
                seen.push(request);
            }
            seen
        });
        (socket, handle)
    }

    /// A `tar` archive with one regular file entry.
    pub(crate) fn tar_regular(name: &str, content: &[u8]) -> Vec<u8> {
        tar_entry(name, b'0', content)
    }

    /// A `tar` archive with one entry of an arbitrary type flag.
    pub(crate) fn tar_entry(name: &str, typeflag: u8, content: &[u8]) -> Vec<u8> {
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"0000644");
        header[108..115].copy_from_slice(b"0000000");
        header[116..123].copy_from_slice(b"0000000");
        let size = format!("{:011o}", content.len());
        header[124..135].copy_from_slice(size.as_bytes());
        header[136] = b'0';
        header[156] = typeflag;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // The checksum field must contain spaces while it is computed.
        header[148..156].copy_from_slice(b"        ");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());

        let mut archive = header.to_vec();
        archive.extend_from_slice(content);
        let padding = (512 - content.len() % 512) % 512;
        archive.extend(std::iter::repeat_n(0_u8, padding));
        archive
    }

    /// `Content-Length` response wrapper.
    pub(crate) fn with_length(body: &[u8]) -> Vec<u8> {
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// Chunked response wrapper splitting `body` into `chunks`.
    pub(crate) fn chunked(body: &[u8], chunks: usize) -> Vec<u8> {
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let size = body.len().div_ceil(chunks.max(1));
        for chunk in body.chunks(size) {
            response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(chunk);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        response
    }
}

#[cfg(test)]
mod tests {
    use super::{testing, *};
    use crate::test_support::Sandbox;

    const JSON: &str = r#"{"type":"codex","access_token":"fixture"}"#;

    fn read(sandbox: &Sandbox, response: Vec<u8>) -> (Result<String, String>, Vec<u8>) {
        let (socket, server) = testing::serve(&sandbox.0, vec![response]);
        let result = read_file(
            &socket,
            "cli-proxy-api",
            "/root/.cli-proxy-api/codex-a.json",
        );
        let requests = server.join().unwrap();
        (result, requests[0].clone())
    }

    #[test]
    fn request_line_encodes_the_container_path_and_host() {
        let sandbox = Sandbox::default();
        let archive = testing::tar_regular("codex-a.json", JSON.as_bytes());
        let (result, request) = read(&sandbox, testing::with_length(&archive));

        assert_eq!(result.unwrap(), JSON);
        let request = String::from_utf8(request).unwrap();
        assert_eq!(
            request.lines().next(),
            Some(
                "GET /v1.41/containers/cli-proxy-api/archive?path=%2Froot%2F.cli-proxy-api%2Fcodex-a.json HTTP/1.1"
            )
        );
        assert!(request.contains("\r\nHost: docker\r\n"));
        assert!(request.contains("\r\nConnection: close\r\n"));
    }

    #[test]
    fn content_length_and_chunked_framing_both_decode() {
        let sandbox = Sandbox::default();
        let archive = testing::tar_regular("codex-a.json", JSON.as_bytes());

        let (length, _) = read(&sandbox, testing::with_length(&archive));
        assert_eq!(length.unwrap(), JSON);

        let (chunked, _) = read(&sandbox, testing::chunked(&archive, 3));
        assert_eq!(chunked.unwrap(), JSON);
    }

    #[test]
    fn metadata_entries_are_skipped_until_the_regular_file() {
        let sandbox = Sandbox::default();
        let mut archive = testing::tar_entry("PaxHeaders.0/", b'x', b"30 path=codex-a.json\n");
        archive.extend(testing::tar_regular("codex-a.json", JSON.as_bytes()));

        let (result, _) = read(&sandbox, testing::with_length(&archive));
        assert_eq!(result.unwrap(), JSON);
    }

    #[test]
    fn error_status_never_echoes_the_daemon_body() {
        let sandbox = Sandbox::default();
        for (status, message) in [
            (404, "Auth file not found in the container"),
            (403, "Docker denied the container read"),
            (500, "Docker API request failed"),
        ] {
            let response = format!(
                "HTTP/1.1 {status} Error\r\nContent-Length: 29\r\n\r\n{{\"message\":\"secret-path-leak\"}}"
            )
            .into_bytes();
            let (result, _) = read(&sandbox, response);
            let error = result.unwrap_err();
            assert!(error.contains(message), "{error}");
            assert!(!error.contains("secret-path-leak"), "{error}");
        }
    }

    #[test]
    fn truncated_and_malformed_archives_are_errors() {
        let sandbox = Sandbox::default();

        // Header claims 4096 bytes, the body stops after the header.
        let mut cut = testing::tar_regular("codex-a.json", &[0; 4096]);
        cut.truncate(512 + 512);
        let (result, _) = read(&sandbox, testing::with_length(&cut));
        assert_eq!(result.unwrap_err(), "Truncated container archive");

        // No tar entry at all: only the end-of-archive marker.
        let (result, _) = read(&sandbox, testing::with_length(&[0; 1024]));
        assert_eq!(
            result.unwrap_err(),
            "Auth file missing from the container archive"
        );

        // Framing damage, not a tar problem.
        let (result, _) = read(&sandbox, b"garbage without a header".to_vec());
        assert_eq!(result.unwrap_err(), MALFORMED);

        // Content-Length larger than the body.
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nshort".to_vec();
        let (result, _) = read(&sandbox, response);
        assert_eq!(result.unwrap_err(), TRUNCATED);
    }

    #[test]
    fn non_utf8_content_and_oversized_responses_are_rejected() {
        let sandbox = Sandbox::default();
        let archive = testing::tar_regular("codex-a.json", &[0xff, 0xfe]);
        let (result, _) = read(&sandbox, testing::with_length(&archive));
        assert_eq!(result.unwrap_err(), "Auth file is not UTF-8");

        let response = testing::with_length(&vec![b'x'; MAX_RESPONSE + 16]);
        let (socket, server) = testing::serve(&sandbox.0, vec![response]);
        let result = read_file(
            &socket,
            "cli-proxy-api",
            "/root/.cli-proxy-api/codex-a.json",
        );
        let _ = server.join();
        assert_eq!(result.unwrap_err(), "Docker response too large");
    }

    #[test]
    fn absent_socket_is_a_socket_error_not_a_tar_error() {
        let sandbox = Sandbox::default();
        let error = read_file(
            &sandbox.0.join("absent.sock"),
            "cli-proxy-api",
            "/root/.cli-proxy-api/codex-a.json",
        )
        .unwrap_err();
        assert_eq!(error, "Cannot reach the Docker socket");
    }

    #[test]
    fn socket_path_defaults_and_honours_the_override() {
        assert_eq!(socket_path_from(None), PathBuf::from(DEFAULT_SOCKET));
        assert_eq!(
            socket_path_from(Some(OsStr::new(""))),
            PathBuf::from(DEFAULT_SOCKET)
        );
        assert_eq!(
            socket_path_from(Some(OsStr::new("/tmp/test.sock"))),
            PathBuf::from("/tmp/test.sock")
        );
    }

    #[test]
    fn percent_encoding_keeps_only_unreserved_bytes() {
        assert_eq!(
            percent_encode("/root/.cli-proxy-api/codex-a.json"),
            "%2Froot%2F.cli-proxy-api%2Fcodex-a.json"
        );
        assert_eq!(percent_encode("a b~c-d_e.f"), "a%20b~c-d_e.f");
    }
}
