use std::io::Cursor;

use rustls::server::Acceptor;
use tokio::io::{AsyncRead, AsyncReadExt};

const TLS_HANDSHAKE: u8 = 22;
const CLIENT_HELLO: u8 = 1;
const ECH_EXTENSION: u16 = 0xfe0d;

#[derive(Debug)]
pub(crate) struct InspectedClientHello {
    pub(crate) wire_bytes: Vec<u8>,
    pub(crate) server_name: Option<String>,
    pub(crate) ech_present: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClientHelloError {
    Invalid,
    TooLarge,
    UnexpectedEof,
}

pub(crate) async fn read_client_hello<R: AsyncRead + Unpin>(
    reader: &mut R,
    initial: Vec<u8>,
    max_bytes: usize,
) -> Result<InspectedClientHello, ClientHelloError> {
    if initial.len() > max_bytes {
        return Err(ClientHelloError::TooLarge);
    }

    let mut wire_bytes = initial;
    let mut acceptor = Acceptor::default();
    let mut fed = 0;
    loop {
        if fed < wire_bytes.len() {
            let mut cursor = Cursor::new(&wire_bytes[fed..]);
            let consumed = acceptor
                .read_tls(&mut cursor)
                .map_err(|_| ClientHelloError::Invalid)?;
            if consumed == 0 {
                return Err(ClientHelloError::Invalid);
            }
            fed += consumed;
        }

        match acceptor.accept() {
            Ok(Some(accepted)) => {
                let server_name = accepted.client_hello().server_name().map(ToOwned::to_owned);
                let ech_present = client_hello_has_ech(&wire_bytes)?;
                return Ok(InspectedClientHello {
                    wire_bytes,
                    server_name,
                    ech_present,
                });
            }
            Ok(None) => {}
            Err(_) => return Err(ClientHelloError::Invalid),
        }

        if wire_bytes.len() == max_bytes {
            return Err(ClientHelloError::TooLarge);
        }
        let mut chunk = [0_u8; 4_096];
        let allowed = (max_bytes - wire_bytes.len()).min(chunk.len());
        let read = reader
            .read(&mut chunk[..allowed])
            .await
            .map_err(|_| ClientHelloError::Invalid)?;
        if read == 0 {
            return Err(ClientHelloError::UnexpectedEof);
        }
        wire_bytes.extend_from_slice(&chunk[..read]);
    }
}

fn client_hello_has_ech(wire_bytes: &[u8]) -> Result<bool, ClientHelloError> {
    let handshake = deframe_client_hello(wire_bytes)?;
    let mut body = handshake.as_slice();
    take(&mut body, 2 + 32)?;
    take_u8_vector(&mut body)?;
    take_u16_vector(&mut body)?;
    take_u8_vector(&mut body)?;
    if body.is_empty() {
        return Ok(false);
    }
    let mut extensions = take_u16_vector(&mut body)?;
    if !body.is_empty() {
        return Err(ClientHelloError::Invalid);
    }
    while !extensions.is_empty() {
        let extension_type = take_u16(&mut extensions)?;
        take_u16_vector(&mut extensions)?;
        if extension_type == ECH_EXTENSION {
            return Ok(true);
        }
    }
    Ok(false)
}

fn deframe_client_hello(wire_bytes: &[u8]) -> Result<Vec<u8>, ClientHelloError> {
    let mut records = wire_bytes;
    let mut handshake = Vec::new();
    let mut expected = None;
    while !records.is_empty() {
        let content_type = take(&mut records, 1)?[0];
        take(&mut records, 2)?;
        let record = take_u16_vector(&mut records)?;
        if content_type != TLS_HANDSHAKE {
            return Err(ClientHelloError::Invalid);
        }
        handshake.extend_from_slice(record);
        if handshake.len() >= 4 && expected.is_none() {
            if handshake[0] != CLIENT_HELLO {
                return Err(ClientHelloError::Invalid);
            }
            let length = (usize::from(handshake[1]) << 16)
                | (usize::from(handshake[2]) << 8)
                | usize::from(handshake[3]);
            expected = Some(
                4_usize
                    .checked_add(length)
                    .ok_or(ClientHelloError::Invalid)?,
            );
        }
        if expected.is_some_and(|length| handshake.len() >= length) {
            break;
        }
    }
    let expected = expected.ok_or(ClientHelloError::Invalid)?;
    if handshake.len() < expected {
        return Err(ClientHelloError::Invalid);
    }
    Ok(handshake[4..expected].to_vec())
}

fn take_u8_vector<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], ClientHelloError> {
    let length = usize::from(take(bytes, 1)?[0]);
    take(bytes, length)
}

fn take_u16_vector<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], ClientHelloError> {
    let length = usize::from(take_u16(bytes)?);
    take(bytes, length)
}

fn take_u16(bytes: &mut &[u8]) -> Result<u16, ClientHelloError> {
    let value = take(bytes, 2)?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn take<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8], ClientHelloError> {
    if bytes.len() < count {
        return Err(ClientHelloError::Invalid);
    }
    let (value, rest) = bytes.split_at(count);
    *bytes = rest;
    Ok(value)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::{CLIENT_HELLO, ECH_EXTENSION, TLS_HANDSHAKE};

    pub(crate) fn push_u16(bytes: &mut Vec<u8>, value: usize) {
        bytes.extend_from_slice(
            &u16::try_from(value)
                .expect("test length fits u16")
                .to_be_bytes(),
        );
    }

    fn extension(extension_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut extension = extension_type.to_be_bytes().to_vec();
        push_u16(&mut extension, payload.len());
        extension.extend_from_slice(payload);
        extension
    }

    pub(crate) fn client_hello(hostname: Option<&str>, ech: bool) -> Vec<u8> {
        client_hello_with_padding(hostname, ech, 0)
    }

    pub(crate) fn client_hello_with_grease(hostname: &str) -> Vec<u8> {
        build_client_hello(Some(hostname), false, 0, true)
    }

    pub(crate) fn client_hello_with_padding(
        hostname: Option<&str>,
        ech: bool,
        padding: usize,
    ) -> Vec<u8> {
        build_client_hello(hostname, ech, padding, false)
    }

    fn build_client_hello(
        hostname: Option<&str>,
        ech: bool,
        padding: usize,
        grease: bool,
    ) -> Vec<u8> {
        let mut extensions = Vec::new();
        if grease {
            extensions.extend_from_slice(&extension(0x0a0a, &[]));
        }
        if let Some(hostname) = hostname {
            let mut server_name = vec![0];
            push_u16(&mut server_name, hostname.len());
            server_name.extend_from_slice(hostname.as_bytes());
            let mut server_name_list = Vec::new();
            push_u16(&mut server_name_list, server_name.len());
            server_name_list.extend_from_slice(&server_name);
            extensions.extend_from_slice(&extension(0, &server_name_list));
        }
        extensions.extend_from_slice(&extension(43, &[2, 3, 4]));
        extensions.extend_from_slice(&extension(13, &[0, 2, 4, 3]));
        if padding > 0 {
            extensions.extend_from_slice(&extension(21, &vec![0; padding]));
        }
        if ech {
            extensions.extend_from_slice(&extension(
                ECH_EXTENSION,
                &[0, 0, 1, 0, 1, 1, 0, 0, 0, 1, 0],
            ));
        }

        let mut body = vec![3, 3];
        body.extend_from_slice(&[7; 32]);
        body.push(0);
        if grease {
            body.extend_from_slice(&[0, 4, 0x0a, 0x0a, 0x13, 0x01]);
        } else {
            body.extend_from_slice(&[0, 2, 0x13, 0x01]);
        }
        body.extend_from_slice(&[1, 0]);
        push_u16(&mut body, extensions.len());
        body.extend_from_slice(&extensions);

        let mut handshake = vec![CLIENT_HELLO];
        let length = u32::try_from(body.len()).expect("test length fits u24");
        handshake.extend_from_slice(&length.to_be_bytes()[1..]);
        handshake.extend_from_slice(&body);

        let mut record = vec![TLS_HANDSHAKE, 3, 1];
        push_u16(&mut record, handshake.len());
        record.extend_from_slice(&handshake);
        record
    }

    pub(crate) fn fragment_record(record: &[u8], at: usize) -> Vec<u8> {
        let payload = &record[5..];
        let mut fragmented = vec![TLS_HANDSHAKE, 3, 1];
        push_u16(&mut fragmented, at);
        fragmented.extend_from_slice(&payload[..at]);
        fragmented.extend_from_slice(&[TLS_HANDSHAKE, 3, 1]);
        push_u16(&mut fragmented, payload.len() - at);
        fragmented.extend_from_slice(&payload[at..]);
        fragmented
    }

    pub(crate) fn fragment_records(record: &[u8], chunk_size: usize) -> Vec<u8> {
        let mut fragmented = Vec::new();
        for payload in record[5..].chunks(chunk_size) {
            fragmented.extend_from_slice(&[TLS_HANDSHAKE, 3, 1]);
            push_u16(&mut fragmented, payload.len());
            fragmented.extend_from_slice(payload);
        }
        fragmented
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{
        client_hello, client_hello_with_grease, client_hello_with_padding, fragment_record,
        fragment_records,
    };
    use super::*;

    fn inspect(wire: &[u8]) -> Result<InspectedClientHello, ClientHelloError> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(async {
                let split = wire.len().min(11);
                let mut remaining = &wire[split..];
                read_client_hello(&mut remaining, wire[..split].to_vec(), 65_536).await
            })
    }

    #[test]
    fn accepts_incremental_fragmented_client_hello() {
        let hello = fragment_record(&client_hello(Some("Example.COM"), false), 17);
        let inspected = inspect(&hello).expect("valid fragmented ClientHello");
        assert_eq!(inspected.wire_bytes, hello);
        assert_eq!(inspected.server_name.as_deref(), Some("example.com"));
        assert!(!inspected.ech_present);
    }

    #[test]
    fn accepts_grease_without_confusing_it_with_ech() {
        let hello = client_hello_with_grease("grease.example");
        let inspected = inspect(&hello).expect("valid ClientHello with GREASE values");
        assert_eq!(inspected.wire_bytes, hello);
        assert_eq!(inspected.server_name.as_deref(), Some("grease.example"));
        assert!(!inspected.ech_present);
    }

    #[test]
    fn reports_ech_and_missing_sni() {
        let missing = inspect(&client_hello(None, false)).expect("valid ClientHello without SNI");
        assert_eq!(missing.server_name, None);
        assert!(!missing.ech_present);

        let ech = inspect(&client_hello(Some("public.example"), true))
            .expect("valid outer ECH ClientHello");
        assert_eq!(ech.server_name.as_deref(), Some("public.example"));
        assert!(ech.ech_present);
    }

    #[test]
    fn enforces_the_outer_byte_bound() {
        let hello = client_hello(Some("example.com"), false);
        let result = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(async { read_client_hello(&mut &[][..], hello, 32).await });
        assert_eq!(result.unwrap_err(), ClientHelloError::TooLarge);
    }

    #[test]
    fn accepts_a_large_client_hello_across_bounded_tls_records() {
        let hello = client_hello_with_padding(Some("large.example"), false, 60_000);
        let hello = fragment_records(&hello, 16_384);
        let inspected = inspect(&hello).expect("large fragmented ClientHello");
        assert_eq!(inspected.wire_bytes, hello);
        assert_eq!(inspected.server_name.as_deref(), Some("large.example"));
    }

    #[test]
    fn every_client_hello_truncation_fails_closed() {
        let hello = client_hello(Some("truncated.example"), true);
        for end in 0..hello.len() {
            assert!(
                inspect(&hello[..end]).is_err(),
                "accepted prefix at byte {end}"
            );
        }
    }

    #[test]
    fn every_client_hello_record_split_is_accepted() {
        let hello = client_hello(Some("fragmented.example"), false);
        for split in 1..hello.len() - 5 {
            let fragmented = fragment_record(&hello, split);
            let inspected = inspect(&fragmented)
                .unwrap_or_else(|error| panic!("rejected split at byte {split}: {error:?}"));
            assert_eq!(inspected.server_name.as_deref(), Some("fragmented.example"));
        }
    }

    #[test]
    fn corrupt_record_and_handshake_lengths_fail_closed() {
        let hello = client_hello(Some("length.example"), false);
        for range in [3..5, 6..9] {
            let mut corrupt = hello.clone();
            corrupt[range].fill(0xff);
            assert!(inspect(&corrupt).is_err());
        }
    }
}
