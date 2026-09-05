//! Shared benchmark oracle; also exercised by a real-denial negative control.

pub(crate) fn assert_connect_success(response: &[u8]) {
    assert_eq!(response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
}
