use agent_client::Client;
use tokio::{io::AsyncWriteExt, net::UnixListener};

#[tokio::test]
async fn readiness_has_a_deadline_and_accepts_a_ready_peer() {
    let path = std::env::temp_dir().join(format!("ac-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    let connecting = tokio::spawn({
        let path = path.clone();
        async move { Client::connect(&path).await }
    });
    let (silent, _) = listener.accept().await.unwrap();
    // Let connect enter the readiness wait before advancing virtual time.
    tokio::task::yield_now().await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(6)).await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), connecting)
        .await
        .expect("silent peer must not leave connect pending")
        .unwrap();
    assert_eq!(result.err().unwrap().code, "daemon_ready_timeout");
    tokio::time::resume();
    drop(silent);
    let ready = tokio::spawn({
        let path = path.clone();
        async move { Client::connect(&path).await }
    });
    let (mut peer, _) = listener.accept().await.unwrap();
    peer.write_all(b"{\"event\":\"ready\"}\n").await.unwrap();
    let (client, _) = ready.await.unwrap().unwrap();
    client.close().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn large_event_burst_lags_by_bytes_before_the_count_limit() {
    let path = std::env::temp_dir().join(format!("ac-burst-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    let connecting = tokio::spawn({
        let path = path.clone();
        async move { Client::connect(&path).await }
    });
    let (mut peer, _) = listener.accept().await.unwrap();
    peer.write_all(b"{\"event\":\"ready\"}\n").await.unwrap();
    let (client, mut events) = connecting.await.unwrap().unwrap();
    let writer = tokio::spawn(async move {
        let line = format!(
            "{{\"event\":\"text_delta\",\"text\":\"{}\"}}\n",
            "x".repeat(512 * 1024)
        );
        for _ in 0..16 {
            if peer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), writer)
        .await
        .unwrap()
        .unwrap();
    // Wait for the reader to finish processing the burst without draining its
    // budget. Writing the last line can complete before it has been queued.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if client
                .request("status", serde_json::json!({}))
                .await
                .err()
                .is_some_and(|e| e.code == "daemon_disconnected")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut received = 0;
    let mut lagged = false;
    while let Some(event) = events.recv().await {
        if event["event"] == "follow_lagged" {
            lagged = true;
        } else {
            received += 1;
        }
    }
    assert!(lagged);
    assert!(
        received > 0 && received < 16,
        "encoded queue exceeded 8 MiB: {received}"
    );
    assert_eq!(
        client
            .request("status", serde_json::json!({}))
            .await
            .err()
            .unwrap()
            .code,
        "daemon_disconnected"
    );
    std::fs::remove_file(path).unwrap();
}
