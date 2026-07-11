//! Shared harness for the C8 integration tests (channel / terminal /
//! SSE): real AppState + router on a temp DB, a real TCP listener for
//! websocket legs, and a fake reeve-agent speaking the actual
//! rev-001/1 frame types from reeve-types.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::Path as FsPath;

use axum::Router;
use rusqlite::params;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::header;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use reeve_server::config::{AuthMode, Config};
use reeve_server::state::AppState;
use reeve_server::{auth, device_tokens, router};
use reeve_types::reeve::channel::{CHANNEL_PATH, CHANNEL_PROTOCOL, ControlFrame};

pub type AgentWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub fn config(data_dir: &FsPath, auth: AuthMode) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        auth,
        session_ttl_secs: 3600,
        registry_endpoint: "registry.example:5000".to_string(),
        durability: reeve_server::config::DurabilityConfig::disabled(),
        zot: None,
        federation: None,
        install_open: false,
    }
}

pub fn app(dir: &FsPath, auth_mode: AuthMode) -> (Router, AppState) {
    let state = reeve_server::bootstrap(config(dir, auth_mode)).expect("bootstrap");
    auth::bootstrap(&state).expect("auth bootstrap");
    (router::build(state.clone()), state)
}

/// Device row + bearer token fixture (enrollment itself is covered by
/// enroll_flow.rs).
pub fn add_device(state: &AppState, id: &str) -> String {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO devices (device_id, hostname, arch, agent_version, enrolled_at)
         VALUES (?1, 'box', 'x86_64', '0.1.0', 0)",
        params![id],
    )
    .unwrap();
    device_tokens::issue(&conn, id).unwrap()
}

/// Serve the router on a real listener (websocket legs need actual
/// TCP; oneshot cannot upgrade).
pub async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    addr
}

/// Open the device channel like reeve-agent does (§4.1: Bearer device
/// token on the upgrade) and complete the hello exchange: expect the
/// server hello first, answer with the agent hello.
pub async fn connect_agent(addr: SocketAddr, token: &str) -> AgentWs {
    let mut ws = connect_agent_raw(addr, token).await.expect("channel upgrade");
    // Server hello once at open (§4.2).
    let hello = recv_control(&mut ws).await;
    let ControlFrame::Hello { protocol, .. } = hello else {
        panic!("first server frame must be hello, got {hello:?}");
    };
    assert_eq!(protocol, CHANNEL_PROTOCOL);
    send_control(
        &mut ws,
        &ControlFrame::Hello {
            protocol: CHANNEL_PROTOCOL.into(),
            extensions: vec![reeve_types::reeve::channel::PURPOSE_TERMINAL.into()],
        },
    )
    .await;
    ws
}

/// Raw upgrade attempt — for the auth-rejection tests.
pub async fn connect_agent_raw(
    addr: SocketAddr,
    token: &str,
) -> Result<AgentWs, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{addr}{CHANNEL_PATH}")
        .into_client_request()
        .unwrap();
    if !token.is_empty() {
        request.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
    }
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;
    Ok(ws)
}

/// Next CONTROL frame (skipping transport ping/pong), with a timeout.
pub async fn recv_control(ws: &mut AgentWs) -> ControlFrame {
    use futures_util::StreamExt as _;
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("control frame within timeout")
            .expect("socket open")
            .expect("frame");
        match msg {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Binary(_) => panic!("expected control frame, got data frame"),
            _ => continue,
        }
    }
}

/// Next DATA frame payload (skipping control ping traffic).
pub async fn recv_binary(ws: &mut AgentWs) -> Vec<u8> {
    use futures_util::StreamExt as _;
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("data frame within timeout")
            .expect("socket open")
            .expect("frame");
        match msg {
            Message::Binary(b) => return b.to_vec(),
            Message::Text(t) => {
                // Skip keepalive pings the server may interleave.
                let frame: ControlFrame = serde_json::from_str(t.as_str()).unwrap();
                match frame {
                    ControlFrame::Ping { .. } => continue,
                    other => panic!("expected data frame, got {other:?}"),
                }
            }
            _ => continue,
        }
    }
}

pub async fn send_control(ws: &mut AgentWs, frame: &ControlFrame) {
    use futures_util::SinkExt as _;
    ws.send(Message::Text(
        serde_json::to_string(frame).unwrap().into(),
    ))
    .await
    .unwrap();
}

pub async fn send_binary(ws: &mut AgentWs, bytes: Vec<u8>) {
    use futures_util::SinkExt as _;
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}

/// Poll until `check` passes (crash-only tests never sleep blind).
pub async fn wait_for(mut check: impl FnMut() -> bool, what: &str) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}
