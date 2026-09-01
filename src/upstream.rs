use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use crate::connect::find_header_end;

const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1_024;
const MAX_RESPONSE_HEADERS: usize = 64;

pub(crate) struct ConnectedStream {
    stream: TcpStream,
    prefix: Vec<u8>,
    prefix_offset: usize,
}

impl ConnectedStream {
    pub(crate) fn direct(stream: TcpStream) -> Self {
        Self::with_prefix(stream, Vec::new())
    }

    fn with_prefix(stream: TcpStream, prefix: Vec<u8>) -> Self {
        Self {
            stream,
            prefix,
            prefix_offset: 0,
        }
    }
}

pub(crate) async fn connect_via(
    proxy: SocketAddr,
    target: SocketAddr,
) -> io::Result<ConnectedStream> {
    let mut stream = TcpStream::connect(proxy).await?;
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;

    let mut bytes = Vec::with_capacity(1_024);
    let end = loop {
        if bytes.len() == MAX_RESPONSE_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upstream proxy response header is too large",
            ));
        }
        let mut chunk = [0_u8; 1_024];
        let remaining = MAX_RESPONSE_HEADER_BYTES - bytes.len();
        let chunk_len = remaining.min(chunk.len());
        let read = stream.read(&mut chunk[..chunk_len]).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "upstream proxy closed before a complete response",
            ));
        }
        let scan_from = bytes.len().saturating_sub(3);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_header_end(&bytes, scan_from) {
            break end;
        }
    };
    validate_response(&bytes[..end])?;
    let prefix = bytes.split_off(end);
    Ok(ConnectedStream::with_prefix(stream, prefix))
}

fn validate_response(bytes: &[u8]) -> io::Result<()> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_RESPONSE_HEADERS];
    let mut response = httparse::Response::new(&mut headers);
    if !matches!(response.parse(bytes), Ok(httparse::Status::Complete(_))) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid upstream proxy response",
        ));
    }
    if !matches!(response.code, Some(200..=299)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "upstream proxy refused CONNECT",
        ));
    }
    Ok(())
}

impl AsyncRead for ConnectedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.prefix_offset < self.prefix.len() {
            let count = buffer
                .remaining()
                .min(self.prefix.len() - self.prefix_offset);
            let end = self.prefix_offset + count;
            buffer.put_slice(&self.prefix[self.prefix_offset..end]);
            self.prefix_offset = end;
            if self.prefix_offset == self.prefix.len() {
                self.prefix = Vec::new();
                self.prefix_offset = 0;
            }
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for ConnectedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_status_must_be_successful() {
        validate_response(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("successful CONNECT response");
        let error = validate_response(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
            .expect_err("proxy refusal");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn malformed_response_is_rejected() {
        let error = validate_response(b"not HTTP\r\n\r\n").expect_err("malformed response");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn incremental_scan_finds_terminators_split_across_reads() {
        let response = b"HTTP/1.1 200 OK\r\nHeader: value\r\n\r\nprefix";
        let expected = response.len() - b"prefix".len();
        for first_read in expected.saturating_sub(3)..expected {
            let mut bytes = response[..first_read].to_vec();
            let scan_from = bytes.len().saturating_sub(3);
            bytes.extend_from_slice(&response[first_read..]);
            assert_eq!(find_header_end(&bytes, scan_from), Some(expected));
        }
    }
}
