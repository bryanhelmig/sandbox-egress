use super::*;

fn local_incomplete_dns_response(query: &[u8]) -> Vec<u8> {
    query[..2].to_vec()
}

fn local_wrong_question_dns_response(query: &[u8]) -> Vec<u8> {
    let question_end = local_dns_question_end(query);
    let mut response = query[..question_end].to_vec();
    response[2..12].copy_from_slice(&[0x81, 0x80, 0, 1, 0, 0, 0, 0, 0, 0]);
    response[13] = b'x';
    response
}

fn local_inflated_answer_count_dns_response(query: &[u8]) -> Vec<u8> {
    let mut response = Vec::with_capacity(12);
    response.extend_from_slice(&query[..2]);
    response.extend_from_slice(&[0x81, 0x80]);
    response.extend_from_slice(&[0, 0, 0xff, 0xff, 0, 0, 0, 0]);
    response
}

fn local_cname_chain_metadata_response(query: &[u8]) -> Vec<u8> {
    const CHAIN: [&[u8]; 8] = [
        b"\x02c0\x05chain\x04test\x00",
        b"\x02c1\x05chain\x04test\x00",
        b"\x02c2\x05chain\x04test\x00",
        b"\x02c3\x05chain\x04test\x00",
        b"\x02c4\x05chain\x04test\x00",
        b"\x02c5\x05chain\x04test\x00",
        b"\x02c6\x05chain\x04test\x00",
        b"\x02c7\x05chain\x04test\x00",
    ];

    let question_end = local_dns_question_end(query);
    let question_name = &query[12..question_end - 4];
    let question_type = u16::from_be_bytes([query[question_end - 4], query[question_end - 3]]);
    let position = CHAIN
        .iter()
        .position(|candidate| *candidate == question_name)
        .expect("query belongs to the controlled CNAME chain");
    let terminal = position == CHAIN.len() - 1;
    let answer_count = u8::from(!terminal || question_type == 1);
    let mut response = Vec::with_capacity(question_end + 32);
    response.extend_from_slice(&query[..2]);
    response.extend_from_slice(&[0x81, 0x80]);
    response.extend_from_slice(&[0, 1, 0, answer_count, 0, 0, 0, 0]);
    response.extend_from_slice(&query[12..question_end]);
    if terminal {
        if question_type == 1 {
            response.extend_from_slice(&[
                0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 169, 254, 169, 254,
            ]);
        }
    } else {
        let target = CHAIN[position + 1];
        response.extend_from_slice(&[0xc0, 0x0c, 0, 5, 0, 1, 0, 0, 0, 60, 0, 15]);
        response.extend_from_slice(target);
    }
    response
}

fn local_cname_loop_response(query: &[u8]) -> Vec<u8> {
    const ALIAS: &[u8] = b"\x04loop\x04test\x00";
    const TARGET: &[u8] = b"\x06target\x04test\x00";

    let question_end = local_dns_question_end(query);
    let question_name = &query[12..question_end - 4];
    let next_name = if question_name == ALIAS {
        TARGET
    } else {
        ALIAS
    };
    let mut response = Vec::with_capacity(question_end + 32);
    response.extend_from_slice(&query[..2]);
    response.extend_from_slice(&[0x81, 0x80]);
    response.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 0]);
    response.extend_from_slice(&query[12..question_end]);
    response.extend_from_slice(&[0xc0, 0x0c, 0, 5, 0, 1, 0, 0, 0, 60]);
    response.extend_from_slice(
        &u16::try_from(next_name.len())
            .expect("bounded CNAME target")
            .to_be_bytes(),
    );
    response.extend_from_slice(next_name);
    response
}

#[test]
fn long_cname_chain_to_metadata_is_rejected_before_dialing() {
    let (address, server) = start_local_dns(16, local_cname_chain_metadata_response);
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_connector(
        ProxyConfig::default()
            .with_dns_server(address)
            .with_dns_cache(0, Duration::ZERO),
        connector,
    )
    .expect("start explicit DNS proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("c0.chain.test", 443),
        )
        .expect("attach DNS lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT c0.chain.test:443 HTTP/1.1\r\nHost: c0.chain.test\r\n\r\n",
    )
    .expect("write aliased CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read aliased DNS denial");

    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("resolved-address-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close DNS lease")
        .usage();
    assert_eq!(final_usage.denied_connections, 1);
    server.join().expect("join local DNS server");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

fn assert_bad_dns_response_is_bounded(
    expected_queries: usize,
    respond: fn(&[u8]) -> Vec<u8>,
    expected_status: &str,
    expected_reason: &str,
) {
    let (address, server) = start_local_dns(expected_queries, respond);
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_connector(
        ProxyConfig::default()
            .with_dns_server(address)
            .with_dns_cache(0, Duration::ZERO),
        connector,
    )
    .expect("start malformed DNS proxy");
    let policy = Policy::builder()
        .allow_host("malformed.test")
        .expect("valid hostname")
        .allow_port(443)
        .dns_timeout(Duration::from_millis(200))
        .handshake_timeout(Duration::from_secs(1))
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach malformed DNS lease");
    let started = Instant::now();
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT malformed.test:443 HTTP/1.1\r\nHost: malformed.test\r\n\r\n",
    )
    .expect("write malformed-DNS CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read malformed DNS denial");

    assert!(response.starts_with(expected_status), "{response}");
    assert!(response.contains(expected_reason), "{response}");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close malformed DNS lease")
        .usage();
    assert_eq!(final_usage.denied_connections, 1);
    server.join().expect("join malformed DNS server");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn malformed_dns_replies_are_bounded_and_never_dialed() {
    assert_bad_dns_response_is_bounded(
        6,
        local_incomplete_dns_response,
        "HTTP/1.1 502",
        "dns-failed",
    );
}

#[test]
fn matching_id_with_wrong_dns_question_is_ignored_until_deadline() {
    assert_bad_dns_response_is_bounded(
        2,
        local_wrong_question_dns_response,
        "HTTP/1.1 504",
        "dns-timeout",
    );
}

#[test]
fn inflated_dns_section_counts_fail_without_dialing() {
    assert_bad_dns_response_is_bounded(
        6,
        local_inflated_answer_count_dns_response,
        "HTTP/1.1 502",
        "dns-failed",
    );
}

#[test]
fn cname_loop_is_bounded_and_never_dialed() {
    let (address, server) = start_local_dns(16, local_cname_loop_response);
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_connector(
        ProxyConfig::default()
            .with_dns_server(address)
            .with_dns_cache(0, Duration::ZERO),
        connector,
    )
    .expect("start looping DNS proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("loop.test", 443),
        )
        .expect("attach looping DNS lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT loop.test:443 HTTP/1.1\r\nHost: loop.test\r\n\r\n",
    )
    .expect("write looping-DNS CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read looping DNS denial");

    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert!(response.contains("dns-failed"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close looping DNS lease")
        .usage();
    assert_eq!(final_usage.denied_connections, 1);
    server.join().expect("join looping DNS server");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}
