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
