//! Opt-in process resource measurements under repeated lease churn.

use std::env;
use std::net::{IpAddr, Ipv4Addr};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};

#[derive(Clone, Copy, Debug)]
struct Resources {
    rss_kib: Option<u64>,
    descriptors: Option<u64>,
    threads: Option<u64>,
}

impl Resources {
    fn sample() -> Self {
        Self {
            rss_kib: rss_kib(),
            descriptors: descriptor_count(),
            threads: thread_count(),
        }
    }
}

#[test]
#[ignore = "resource soak is opt-in; run scripts/measure-resources.sh"]
fn identity_churn_has_bounded_process_resources() {
    let runs_per_batch = env_number("SANDBOX_EGRESS_SOAK_RUNS", 2_000);
    let batches = env_number("SANDBOX_EGRESS_SOAK_BATCHES", 4);
    assert!(runs_per_batch > 0 && batches > 0);
    assert!(runs_per_batch.saturating_mul(batches) < 0x00ff_ffff);

    let process_start = Resources::sample();
    let proxy =
        Proxy::start(ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO))
            .expect("start proxy");
    thread::sleep(Duration::from_millis(25));
    let proxy_start = Resources::sample();
    let started = Instant::now();

    eprintln!(
        "resource_soak event=start runs_per_batch={runs_per_batch} batches={batches} process_rss_kib={:?} process_fds={:?} process_threads={:?} proxy_rss_kib={:?} proxy_fds={:?} proxy_threads={:?}",
        process_start.rss_kib,
        process_start.descriptors,
        process_start.threads,
        proxy_start.rss_kib,
        proxy_start.descriptors,
        proxy_start.threads,
    );

    for batch in 0..batches {
        for offset in 0..runs_per_batch {
            let sequence = batch.saturating_mul(runs_per_batch) + offset + 1;
            let identity = PeerIdentity::SourceIp(churn_address(sequence));
            let lease = proxy
                .attach(identity, Policy::builder().build().expect("valid policy"))
                .expect("attach churn lease");
            lease
                .close(Instant::now() + Duration::from_secs(2))
                .expect("close churn lease");
        }

        // Release commands are asynchronous with respect to close returning.
        thread::sleep(Duration::from_millis(25));
        let current = Resources::sample();
        eprintln!(
            "resource_soak event=batch batch={} completed={} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
            batch + 1,
            (batch + 1).saturating_mul(runs_per_batch),
            started.elapsed().as_millis(),
            current.rss_kib,
            current.descriptors,
            current.threads,
        );
        assert_stable_non_memory_resources(proxy_start, current);
    }

    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
    thread::sleep(Duration::from_millis(25));
    let finished = Resources::sample();
    eprintln!(
        "resource_soak event=finish completed={} elapsed_ms={} rss_kib={:?} fds={:?} threads={:?}",
        runs_per_batch.saturating_mul(batches),
        started.elapsed().as_millis(),
        finished.rss_kib,
        finished.descriptors,
        finished.threads,
    );
    assert_stable_non_memory_resources(process_start, finished);
}

fn env_number(name: &str, default: usize) -> usize {
    env::var(name).ok().map_or(default, |value| {
        value.parse().expect("numeric soak setting")
    })
}

fn churn_address(sequence: usize) -> IpAddr {
    let sequence = u32::try_from(sequence).expect("bounded churn sequence");
    let octets = sequence.to_be_bytes();
    IpAddr::V4(Ipv4Addr::new(10, octets[1], octets[2], octets[3]))
}

fn assert_stable_non_memory_resources(baseline: Resources, current: Resources) {
    if let (Some(baseline), Some(current)) = (baseline.descriptors, current.descriptors) {
        assert!(
            current <= baseline + 2,
            "descriptor growth: baseline={baseline}, current={current}"
        );
    }
    if let (Some(baseline), Some(current)) = (baseline.threads, current.threads) {
        assert!(
            current <= baseline + 2,
            "thread growth: baseline={baseline}, current={current}"
        );
    }
}

#[cfg(target_os = "linux")]
fn rss_kib() -> Option<u64> {
    proc_status_number("VmRSS:")
}

#[cfg(target_os = "macos")]
fn rss_kib() -> Option<u64> {
    command_number("ps", &["-o", "rss=", "-p", &std::process::id().to_string()])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rss_kib() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn descriptor_count() -> Option<u64> {
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(|entries| entries.count() as u64)
}

#[cfg(target_os = "macos")]
fn descriptor_count() -> Option<u64> {
    command_line_count("lsof", &["-p", &std::process::id().to_string()])
        .map(|lines| lines.saturating_sub(1))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn descriptor_count() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn thread_count() -> Option<u64> {
    proc_status_number("Threads:")
}

#[cfg(target_os = "macos")]
fn thread_count() -> Option<u64> {
    command_line_count("ps", &["-M", "-p", &std::process::id().to_string()])
        .map(|lines| lines.saturating_sub(1))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn thread_count() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn proc_status_number(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(target_os = "macos")]
fn command_number(program: &str, arguments: &[&str]) -> Option<u64> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output.status.success().then_some(())?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

#[cfg(target_os = "macos")]
fn command_line_count(program: &str, arguments: &[&str]) -> Option<u64> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output.status.success().then_some(())?;
    Some(String::from_utf8(output.stdout).ok()?.lines().count() as u64)
}
