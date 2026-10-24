//! Minimal Socket.IO v4 (Engine.IO v4) client over WebSocket.
//!
//! Only what Voiceitt needs: connect to the default namespace with an auth
//! payload, receive JSON events, send JSON events and *binary events with a
//! trailing text argument* (`stream_audio_samples`, `[bytes, "int16"]`),
//! answer pings, disconnect. The available Rust crate cannot send mixed
//! binary/text argument lists, hence this ~150-line client.
//!
//! Wire format recap (Engine.IO packet type + Socket.IO packet type):
//! `0{open}` server hello · `2` ping / `3` pong · `40{auth}` connect ·
//! `40{sid}` connected · `44{err}` connect error · `42["ev",args…]` event ·
//! `451-["ev",{"_placeholder":true,"num":0},"int16"]` + 1 binary frame ·
//! `41` disconnect.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};

type Sink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

#[derive(Debug, Clone, PartialEq)]
pub enum SioEvent {
    Connected,
    /// Named event with its JSON arguments (event name removed).
    Event {
        name: String,
        args: Vec<serde_json::Value>,
    },
    ConnectError(String),
    Disconnected,
    Transport(String),
}

pub struct SioClient {
    sink: Arc<Mutex<Sink>>,
    pub events: mpsc::Receiver<SioEvent>,
    reader: tokio::task::JoinHandle<()>,
}

/// Encode a text event packet: `42["name",args…]`.
pub fn encode_event(name: &str, args: &[serde_json::Value]) -> String {
    let mut arr = vec![serde_json::Value::String(name.to_string())];
    arr.extend_from_slice(args);
    format!("42{}", serde_json::Value::Array(arr))
}

/// Encode a binary event with one attachment and optional trailing text
/// args: `451-["name",{"_placeholder":true,"num":0},…]`. The binary frame
/// must follow immediately.
pub fn encode_binary_event(name: &str, trailing: &[serde_json::Value]) -> String {
    let mut arr = vec![
        serde_json::Value::String(name.to_string()),
        serde_json::json!({"_placeholder": true, "num": 0}),
    ];
    arr.extend_from_slice(trailing);
    format!("451-{}", serde_json::Value::Array(arr))
}

/// Decode an incoming text packet into an event. `None` for packets the
/// caller does not need (open hello, pong, acks).
pub fn decode_packet(text: &str) -> Option<SioEvent> {
    let rest = text.strip_prefix('4')?; // Engine.IO "message"
    if let Some(payload) = rest.strip_prefix('0') {
        let _ = payload;
        return Some(SioEvent::Connected);
    }
    if let Some(payload) = rest.strip_prefix('4') {
        return Some(SioEvent::ConnectError(payload.to_string()));
    }
    if rest.starts_with('1') {
        return Some(SioEvent::Disconnected);
    }
    let payload = rest.strip_prefix('2')?;
    // Optional ack id digits before '['.
    let json_start = payload.find('[')?;
    let arr: Vec<serde_json::Value> = serde_json::from_str(&payload[json_start..]).ok()?;
    let mut it = arr.into_iter();
    let name = it.next()?.as_str()?.to_string();
    Some(SioEvent::Event {
        name,
        args: it.collect(),
    })
}

impl SioClient {
    /// `base` like `https://web.voiceitt.com`, `auth` is the Socket.IO auth object.
    pub async fn connect(
        base: &str,
        auth: serde_json::Value,
        tls: tokio_tungstenite::Connector,
    ) -> Result<Self, String> {
        let ws_url = format!(
            "{}/socket.io/?EIO=4&transport=websocket",
            base.replace("https://", "wss://").replace("http://", "ws://")
        );
        let req = ws_url.as_str().into_client_request().map_err(|e| e.to_string())?;
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(tls)),
        )
        .await
        .map_err(|_| "socket.io handshake timeout".to_string())?
        .map_err(|e| e.to_string())?;
        let (sink, mut source) = ws.split();
        let sink = Arc::new(Mutex::new(sink));
        let (tx, rx) = mpsc::channel(128);

        // Engine.IO open packet first.
        match tokio::time::timeout(Duration::from_secs(10), source.next()).await {
            Ok(Some(Ok(Message::Text(t)))) if t.as_str().starts_with('0') => debug!("engine.io open: {}", t.as_str()),
            other => return Err(format!("unexpected engine.io hello: {other:?}")),
        }
        // Socket.IO connect to "/" with auth.
        sink.lock()
            .await
            .send(Message::text(format!("40{auth}")))
            .await
            .map_err(|e| e.to_string())?;

        let reader = {
            let sink = sink.clone();
            tokio::spawn(async move {
                while let Some(msg) = source.next().await {
                    match msg {
                        Ok(Message::Text(t)) => {
                            let s = t.as_str();
                            if s == "2" {
                                if sink.lock().await.send(Message::text("3")).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                            if let Some(ev) = decode_packet(s) {
                                let stop = matches!(ev, SioEvent::Disconnected | SioEvent::ConnectError(_));
                                if tx.send(ev).await.is_err() || stop {
                                    break;
                                }
                            }
                        }
                        Ok(Message::Close(_)) => {
                            let _ = tx.send(SioEvent::Disconnected).await;
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = tx.send(SioEvent::Transport(e.to_string())).await;
                            break;
                        }
                    }
                }
            })
        };
        Ok(Self {
            sink,
            events: rx,
            reader,
        })
    }

    pub async fn emit(&self, name: &str, args: &[serde_json::Value]) -> Result<(), String> {
        self.sink
            .lock()
            .await
            .send(Message::text(encode_event(name, args)))
            .await
            .map_err(|e| e.to_string())
    }

    /// Emit `[bytes, trailing…]` as a binary event.
    pub async fn emit_binary(&self, name: &str, bytes: Vec<u8>, trailing: &[serde_json::Value]) -> Result<(), String> {
        let mut sink = self.sink.lock().await;
        sink.send(Message::text(encode_binary_event(name, trailing)))
            .await
            .map_err(|e| e.to_string())?;
        sink.send(Message::binary(bytes)).await.map_err(|e| e.to_string())
    }

    pub async fn disconnect(self) {
        {
            let mut sink = self.sink.lock().await;
            let _ = sink.send(Message::text("41")).await;
            let _ = sink.send(Message::Close(None)).await;
        }
        if tokio::time::timeout(Duration::from_secs(2), self.reader).await.is_err() {
            warn!("socket.io reader did not finish");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_text_and_binary_events() {
        assert_eq!(
            encode_event("set_options", &[serde_json::json!({"run_itn": true})]),
            r#"42["set_options",{"run_itn":true}]"#
        );
        assert_eq!(
            encode_binary_event("stream_audio_samples", &[serde_json::json!("int16")]),
            r#"451-["stream_audio_samples",{"_placeholder":true,"num":0},"int16"]"#
        );
    }

    #[test]
    fn decodes_incoming_packets() {
        assert_eq!(decode_packet(r#"40{"sid":"abc"}"#), Some(SioEvent::Connected));
        assert!(matches!(
            decode_packet(r#"44{"message":"bad token"}"#),
            Some(SioEvent::ConnectError(_))
        ));
        assert_eq!(decode_packet("41"), Some(SioEvent::Disconnected));
        let ev =
            decode_packet(r#"42["partial_recognition",{"segment_id":3,"text":"hal","unstable_text":"lo"}]"#).unwrap();
        match ev {
            SioEvent::Event { name, args } => {
                assert_eq!(name, "partial_recognition");
                assert_eq!(args[0]["segment_id"], 3);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            decode_packet(r#"427["ack_event",1]"#).map(|e| matches!(e, SioEvent::Event { .. })),
            Some(true)
        );
        assert_eq!(decode_packet("3"), None);
    }
}
