use super::*;
use crate::DnsAddressFamily;

struct RecordingConnector(mpsc::Sender<SocketAddr>);

impl TestConnector for RecordingConnector {
    fn connect(
        &self,
        address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send + '_>> {
        self.0.send(address).unwrap();
        Box::pin(async { Err(io::Error::other("recorded test dial")) })
    }
}

fn query_type(query: &[u8]) -> u16 {
    let end = local_dns_question_end(query);
    u16::from_be_bytes([query[end - 4], query[end - 3]])
}

fn dual_stack_response(query: &[u8]) -> Vec<u8> {
    let end = local_dns_question_end(query);
    let kind = query_type(query);
    let bytes = match kind {
        1 => vec![93, 184, 216, 34],
        28 => "2606:4700:4700::1111"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets()
            .to_vec(),
        _ => panic!("unexpected DNS question type {kind}"),
    };
    let mut response = query[..end].to_vec();
    response[2..12].copy_from_slice(&[0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]);
    response.extend_from_slice(&[0xc0, 0x0c]);
    response.extend_from_slice(&kind.to_be_bytes());
    response.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
    response.extend_from_slice(&u16::try_from(bytes.len()).unwrap().to_be_bytes());
    response.extend(bytes);
    response
}

fn a_only_response(query: &[u8]) -> Vec<u8> {
    assert_eq!(query_type(query), 1, "IPv4-only must never query AAAA");
    dual_stack_response(query)
}

fn aaaa_only_response(query: &[u8]) -> Vec<u8> {
    assert_eq!(query_type(query), 28, "IPv6-only must never query A");
    dual_stack_response(query)
}

#[test]
fn real_dns_family_selection_precedes_denials_and_cache_rechecks_each_lease() {
    for (family, respond, excluded, expected) in [
        (
            DnsAddressFamily::Ipv4Only,
            a_only_response as fn(&[u8]) -> Vec<u8>,
            "::/0",
            vec!["93.184.216.34:443"],
        ),
        (
            DnsAddressFamily::Ipv6Only,
            aaaa_only_response as fn(&[u8]) -> Vec<u8>,
            "0.0.0.0/0",
            vec!["[2606:4700:4700::1111]:443"],
        ),
        (
            DnsAddressFamily::Both,
            dual_stack_response as fn(&[u8]) -> Vec<u8>,
            "192.0.2.0/24",
            vec!["[2606:4700:4700::1111]:443", "93.184.216.34:443"],
        ),
    ] {
        let (dns, server) = start_local_dns(expected.len(), respond);
        let (tx, rx) = mpsc::channel();
        let proxy = Proxy::start_with_test_connector(
            ProxyConfig::default()
                .with_dns_server(dns)
                .with_dns_address_family(family)
                .with_dns_cache(8, Duration::from_secs(60))
                .with_denied_network(excluded.parse().unwrap()),
            Arc::new(RecordingConnector(tx)),
        )
        .unwrap();
        let identity = PeerIdentity::SourceIp(std::net::Ipv4Addr::LOCALHOST.into());
        let lease = proxy
            .attach(identity.clone(), proxy_floor_policy())
            .unwrap();
        assert!(proxy_floor_request(&proxy, "floor.test").contains("dial-failed"));
        let actual: Vec<_> = rx.try_iter().collect();
        let expected: Vec<SocketAddr> = expected.iter().map(|s| s.parse().unwrap()).collect();
        assert_eq!(actual, expected, "{family:?}");
        lease
            .close(Instant::now() + Duration::from_secs(2))
            .unwrap();
        server.join().unwrap(); // A second successful lookup must come from cache.

        let policy = Policy::builder()
            .allow_host("floor.test")
            .unwrap()
            .allow_port(443)
            .deny_network("0.0.0.0/0".parse().unwrap())
            .deny_network("::/0".parse().unwrap())
            .build()
            .unwrap();
        let lease = proxy.attach(identity, policy).unwrap();
        assert!(proxy_floor_request(&proxy, "floor.test").contains("resolved-address-denied"));
        assert_eq!(rx.try_iter().count(), 0);
        lease
            .close(Instant::now() + Duration::from_secs(2))
            .unwrap();
        proxy
            .shutdown(Instant::now() + Duration::from_secs(2))
            .unwrap();
    }
}

fn request_with_answers(
    config: ProxyConfig,
    answers: &[&str],
    authority: &str,
) -> (String, Vec<SocketAddr>) {
    let (tx, rx) = mpsc::channel();
    let proxy = Proxy::start_with_test_backends(
        config,
        Arc::new(FixedAnswerResolver(
            answers.iter().map(|s| s.parse().unwrap()).collect(),
        )),
        Arc::new(RecordingConnector(tx)),
    )
    .unwrap();
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(std::net::Ipv4Addr::LOCALHOST.into()),
            proxy_floor_policy(),
        )
        .unwrap();
    let response = proxy_floor_request(&proxy, authority);
    lease
        .close(Instant::now() + Duration::from_secs(2))
        .unwrap();
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .unwrap();
    (response, rx.try_iter().collect())
}

#[test]
fn family_filter_runs_before_answer_ceiling_and_address_canonicalization() {
    for (family, answers, excluded, expected) in [
        (
            DnsAddressFamily::Ipv4Only,
            [
                "::ffff:93.184.216.34",
                "64:ff9b::5db8:d822",
                "93.184.216.35",
            ],
            "::/0",
            "93.184.216.35:443",
        ),
        (
            DnsAddressFamily::Ipv6Only,
            ["93.184.216.34", "93.184.216.35", "2606:4700:4700::1111"],
            "0.0.0.0/0",
            "[2606:4700:4700::1111]:443",
        ),
    ] {
        let (response, dials) = request_with_answers(
            ProxyConfig::default()
                .with_dns_address_family(family)
                .with_max_resolved_addresses(1)
                .with_denied_network(excluded.parse().unwrap()),
            &answers,
            "floor.test",
        );
        assert!(response.contains("dial-failed"), "{response}");
        assert_eq!(dials, vec![expected.parse::<SocketAddr>().unwrap()]);
    }
}

#[test]
fn excluded_family_only_is_empty_and_never_dialed() {
    for (family, answers) in [
        (DnsAddressFamily::Ipv4Only, ["2606:4700:4700::1111"]),
        (DnsAddressFamily::Ipv6Only, ["93.184.216.34"]),
    ] {
        let (response, dials) = request_with_answers(
            ProxyConfig::default().with_dns_address_family(family),
            &answers,
            "floor.test",
        );
        assert!(response.contains("dns-empty"), "{response}");
        assert!(dials.is_empty());
    }
}

#[test]
fn selected_family_still_rejects_oversized_answers() {
    let (response, dials) = request_with_answers(
        ProxyConfig::default()
            .with_dns_address_family(DnsAddressFamily::Ipv4Only)
            .with_max_resolved_addresses(1),
        &["2606:4700:4700::1111", "93.184.216.34", "93.184.216.35"],
        "floor.test",
    );
    assert!(response.contains("dns-answer-too-large"), "{response}");
    assert!(dials.is_empty());
}

#[test]
fn one_denied_selected_address_still_poisons_the_whole_answer() {
    for family in [
        DnsAddressFamily::Both,
        DnsAddressFamily::Ipv4Only,
        DnsAddressFamily::Ipv6Only,
    ] {
        let blocked = if family == DnsAddressFamily::Ipv4Only {
            "93.184.216.34"
        } else {
            "::ffff:93.184.216.34"
        };
        let allowed = if family == DnsAddressFamily::Ipv4Only {
            "93.184.216.35"
        } else {
            "2606:4700:4700::1111"
        };
        for answers in [[allowed, blocked], [blocked, allowed]] {
            let (response, dials) = request_with_answers(
                ProxyConfig::default()
                    .with_dns_address_family(family)
                    .with_denied_network("93.184.216.34/32".parse().unwrap()),
                &answers,
                "floor.test",
            );
            assert!(response.contains("proxy-network-denied"), "{response}");
            assert!(dials.is_empty());
        }
    }
}

#[test]
fn literal_destinations_remain_subject_to_network_rules_not_dns_family() {
    for denied in [false, true] {
        let mut config = ProxyConfig::default().with_dns_address_family(DnsAddressFamily::Ipv4Only);
        if denied {
            config = config.with_denied_network("::/0".parse().unwrap());
        }
        let (response, dials) = request_with_answers(config, &[], "[2606:4700:4700::1111]");
        assert!(response.contains(if denied {
            "proxy-network-denied"
        } else {
            "dial-failed"
        }));
        assert_eq!(dials.len(), usize::from(!denied));
    }
}

#[test]
fn family_setting_applies_to_system_and_explicit_resolvers_with_unchanged_default() {
    use hickory_resolver::config::LookupIpStrategy;
    for server in [None, Some("127.0.0.1:5353".parse().unwrap())] {
        let mut config = ProxyConfig::default();
        if let Some(server) = server {
            config = config.with_dns_server(server);
        }
        assert_eq!(config.dns_address_family, DnsAddressFamily::Both);
        for (family, strategy) in [
            (DnsAddressFamily::Both, LookupIpStrategy::default()),
            (DnsAddressFamily::Ipv4Only, LookupIpStrategy::Ipv4Only),
            (DnsAddressFamily::Ipv6Only, LookupIpStrategy::Ipv6Only),
        ] {
            let resolver =
                build_system_resolver(&config.clone().with_dns_address_family(family)).unwrap();
            assert_eq!(resolver.options().ip_strategy, strategy);
        }
    }
}
