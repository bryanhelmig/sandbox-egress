use http::uri::Authority;

const MAX_CONNECT_HEADERS: usize = 64;

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
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() {
        return Err("missing-host");
    }
    if port == 0 {
        return Err("invalid-port");
    }
    Ok(ConnectRequest {
        host: host.to_owned(),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect_with_headers(count: usize) -> Vec<u8> {
        let mut request = b"CONNECT example.com:443 HTTP/1.1\r\n".to_vec();
        for index in 0..count {
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
        let request = parse_connect(b"CONNECT [2001:db8::1]:443 HTTP/1.1\r\n\r\n")
            .expect("valid IPv6 CONNECT");
        assert_eq!(request.host, "2001:db8::1");
        assert_eq!(request.port, 443);
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
    fn header_count_boundary_is_explicit() {
        parse_connect(&connect_with_headers(MAX_CONNECT_HEADERS))
            .expect("the configured header slots fit the parser bound");
        assert_eq!(
            parse_connect(&connect_with_headers(MAX_CONNECT_HEADERS + 1)).unwrap_err(),
            "too-many-headers"
        );
    }
}
