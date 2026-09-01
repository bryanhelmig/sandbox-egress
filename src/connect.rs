use std::io;
use std::net::{IpAddr, Ipv6Addr};

use http::uri::Authority;
use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_CONNECT_HEADERS: usize = 64;

#[inline]
pub(crate) fn find_header_end(bytes: &[u8], scan_from: usize) -> Option<usize> {
    bytes[scan_from..]
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| scan_from + index + 4)
}

pub(crate) struct HeaderBlock {
    pub(crate) bytes: Vec<u8>,
    pub(crate) end: usize,
}

// Keep the hostile-input scan behind a stable code-generation boundary.
// Whole-program LTO otherwise coupled its loop layout to unrelated policy
// constructor changes; the committed 1 MiB benchmark reproduced the effect.
#[inline(never)]
pub(crate) async fn read_bounded_header<R>(
    stream: &mut R,
    max: usize,
    chunk_bytes: usize,
) -> io::Result<HeaderBlock>
where
    R: AsyncRead + Unpin,
{
    debug_assert!((1..=4_096).contains(&chunk_bytes));
    // Keep ordinary CONNECT headers in one allocation without reserving a
    // full read chunk for every concurrent handshake.
    let mut bytes = Vec::with_capacity(max.min(256));
    let mut chunk = [0_u8; 4_096];
    loop {
        if bytes.len() >= max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header too large",
            ));
        }
        let allowed = (max - bytes.len()).min(chunk_bytes);
        let read = stream.read(&mut chunk[..allowed]).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "header ended early",
            ));
        }
        let scan_from = bytes.len().saturating_sub(3);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_header_end(&bytes, scan_from) {
            return Ok(HeaderBlock { bytes, end });
        }
    }
}

#[derive(Debug)]
pub(crate) struct ConnectRequest {
    pub(crate) host: String,
    pub(crate) port: u16,
}

pub(crate) fn parse_connect(bytes: &[u8]) -> Result<ConnectRequest, &'static str> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_CONNECT_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(bytes) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err("incomplete-header"),
        Err(httparse::Error::TooManyHeaders) => return Err("too-many-headers"),
        Err(_) => return Err("malformed-header"),
    }
    if request.method != Some("CONNECT") {
        return Err("connect-required");
    }
    if !matches!(request.version, Some(0 | 1)) {
        return Err("unsupported-http-version");
    }
    let target = request.path.ok_or("missing-authority")?;
    if target.contains('@') {
        return Err("userinfo-not-allowed");
    }
    let authority: Authority = target.parse().map_err(|_| "invalid-authority")?;
    let port = authority.port_u16().ok_or("missing-port")?;
    let host = authority_host(authority.host()).map_err(|()| "invalid-ipv6-literal")?;
    if host.is_empty() {
        return Err("missing-host");
    }
    if port == 0 {
        return Err("invalid-port");
    }
    validate_host_header(&request, host, port)?;
    Ok(ConnectRequest {
        host: host.to_owned(),
        port,
    })
}

fn validate_host_header(
    request: &httparse::Request<'_, '_>,
    connect_host: &str,
    connect_port: u16,
) -> Result<(), &'static str> {
    let mut values = request
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("host"));
    let Some(value) = values.next() else {
        return if request.version == Some(1) {
            Err("missing-host-header")
        } else {
            Ok(())
        };
    };
    if values.next().is_some() {
        return Err("duplicate-host-header");
    }

    let value = std::str::from_utf8(value.value).map_err(|_| "invalid-host-header")?;
    if value.contains('@') || value.ends_with(':') {
        return Err("invalid-host-header");
    }
    let authority: Authority = value.parse().map_err(|_| "invalid-host-header")?;
    let host = authority_host(authority.host()).map_err(|()| "invalid-host-header")?;
    if !hosts_equivalent(connect_host, host)
        || authority
            .port_u16()
            .is_some_and(|port| port != connect_port)
    {
        return Err("host-header-mismatch");
    }
    Ok(())
}

fn authority_host(host: &str) -> Result<&str, ()> {
    if let Some(host) = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
    {
        host.parse::<Ipv6Addr>().map_err(|_| ())?;
        Ok(host)
    } else {
        Ok(host)
    }
}

fn hosts_equivalent(left: &str, right: &str) -> bool {
    match (left.parse::<IpAddr>(), right.parse::<IpAddr>()) {
        (Ok(left), Ok(right)) => left == right,
        (Err(_), Err(_)) => left
            .strip_suffix('.')
            .unwrap_or(left)
            .eq_ignore_ascii_case(right.strip_suffix('.').unwrap_or(right)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect_with_headers(count: usize) -> Vec<u8> {
        let mut request = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n".to_vec();
        for index in 1..count {
            request.extend_from_slice(format!("x-{index}: value\r\n").as_bytes());
        }
        request.extend_from_slice(b"\r\n");
        request
    }

    #[test]
    fn accepts_connect_authority() {
        let request =
            parse_connect(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n")
                .expect("valid CONNECT");
        assert_eq!(request.host, "example.com");
        assert_eq!(request.port, 443);
    }

    #[test]
    fn rejects_ambiguous_http_11_host_fields() {
        for (request, reason) in [
            (
                "CONNECT example.com:443 HTTP/1.1\r\n\r\n",
                "missing-host-header",
            ),
            (
                "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\nhOsT: example.com\r\n\r\n",
                "duplicate-host-header",
            ),
            (
                "CONNECT example.com:443 HTTP/1.1\r\nHost: user@example.com\r\n\r\n",
                "invalid-host-header",
            ),
            (
                "CONNECT example.com:443 HTTP/1.1\r\nHost: forbidden.example\r\n\r\n",
                "host-header-mismatch",
            ),
            (
                "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:8443\r\n\r\n",
                "host-header-mismatch",
            ),
        ] {
            assert_eq!(
                parse_connect(request.as_bytes()).unwrap_err(),
                reason,
                "unexpected result for {request:?}"
            );
        }
    }

    #[test]
    fn accepts_compatible_host_field_spellings() {
        for request in [
            "CONNECT example.com:443 HTTP/1.1\r\nHost: EXAMPLE.COM\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
            "CONNECT [2001:db8::1]:443 HTTP/1.1\r\nHost: [2001:0db8::1]\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.0\r\n\r\n",
        ] {
            parse_connect(request.as_bytes()).expect("compatible Host field");
        }
    }

    #[test]
    fn rejects_plain_http() {
        assert_eq!(
            parse_connect(b"GET http://example.com/ HTTP/1.1\r\n\r\n").unwrap_err(),
            "connect-required"
        );
    }

    #[test]
    fn rejects_userinfo_in_connect_authority() {
        assert_eq!(
            parse_connect(b"CONNECT user@example.com:443 HTTP/1.1\r\n\r\n").unwrap_err(),
            "userinfo-not-allowed"
        );
    }

    #[test]
    fn normalizes_bracketed_ipv6() {
        let request =
            parse_connect(b"CONNECT [2001:db8::1]:443 HTTP/1.1\r\nHost: [2001:db8::1]\r\n\r\n")
                .expect("valid IPv6 CONNECT");
        assert_eq!(request.host, "2001:db8::1");
        assert_eq!(request.port, 443);
    }

    #[test]
    fn rejects_bracketed_hosts_that_are_not_ipv6() {
        for authority in [
            "[example.com]:443",
            "[127.0.0.1]:443",
            "[v1.example]:443",
            "[fe80::1%25lo]:443",
        ] {
            let request = format!("CONNECT {authority} HTTP/1.1\r\n\r\n");
            assert_eq!(
                parse_connect(request.as_bytes()).unwrap_err(),
                "invalid-ipv6-literal",
                "unexpected result for {authority:?}"
            );
        }
    }

    #[test]
    fn rejects_ambiguous_connect_authorities() {
        for authority in [
            "example.com",
            "example.com:65536",
            "example.com:-1",
            "example.com:0",
            "example.com:443:444",
            "example.com:443/path",
            "example.com:443?query",
            "example.com:443#fragment",
            "[::1]443",
            "[::1]:443/path",
            "user@example.com:443",
            ":443",
        ] {
            let request = format!("CONNECT {authority} HTTP/1.1\r\n\r\n");
            assert!(
                parse_connect(request.as_bytes()).is_err(),
                "accepted ambiguous authority {authority:?}"
            );
        }
    }

    #[test]
    fn rejects_control_and_parser_differential_shapes() {
        for (request, reason) in [
            (
                &b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\nX-Test: one\r\n two\r\n\r\n"[..],
                "malformed-header",
            ),
            (
                &b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\nX\0: value\r\n\r\n"[..],
                "malformed-header",
            ),
            (
                &b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\nX-Test: val\0ue\r\n\r\n"[..],
                "malformed-header",
            ),
            (
                &b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\nX-Test: val\x1fue\r\n\r\n"[..],
                "malformed-header",
            ),
            (
                &b"CONNECT ex\xc3\xa4mple.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n"[..],
                "invalid-authority",
            ),
            (
                &b"CONNECT example.com:443 HTTP/1.1\r\nHost: ex\xc3\xa4mple.com\r\n\r\n"[..],
                "invalid-host-header",
            ),
            (
                &b"CONNECT example.com:443 HTTP/1.1\r\n Host: example.com\r\n\r\n"[..],
                "malformed-header",
            ),
            (
                &b"CONNECT example.com:443 HTTP/1.1\r\nHost : example.com\r\n\r\n"[..],
                "malformed-header",
            ),
        ] {
            assert_eq!(
                parse_connect(request).unwrap_err(),
                reason,
                "unexpected result for {request:?}"
            );
        }
    }

    #[test]
    fn header_count_boundary_is_explicit() {
        parse_connect(&connect_with_headers(MAX_CONNECT_HEADERS))
            .expect("the configured header slots fit the parser bound");
        assert_eq!(
            parse_connect(&connect_with_headers(MAX_CONNECT_HEADERS + 1)).unwrap_err(),
            "too-many-headers"
        );
    }
}
