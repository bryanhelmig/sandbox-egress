use super::*;

#[test]
fn expired_deadline_wins_without_polling_ready_work() {
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let polled = std::sync::atomic::AtomicBool::new(false);
        let result =
            complete_before_deadline(TokioInstant::now() - Duration::from_millis(1), async {
                polled.store(true, Ordering::Release);
                7
            })
            .await;

        assert_eq!(result, None);
        assert!(!polled.load(Ordering::Acquire));
    });
}

#[test]
fn blocked_connect_response_write_stops_at_deadline() {
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let result =
            write_connect_success(&mut writer, TokioInstant::now() + Duration::from_millis(20))
                .await
                .expect("bounded response write");
        assert!(!result, "blocked response unexpectedly completed");

        writer.shutdown().await.expect("close response writer");
        let mut observed = Vec::new();
        reader
            .read_to_end(&mut observed)
            .await
            .expect("read bounded response prefix");
        assert!(
            observed.len() < CONNECT_SUCCESS_RESPONSE.len(),
            "complete response escaped the deadline"
        );
    });
}
