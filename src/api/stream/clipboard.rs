//! Clipboard text is transported separately from keyboard input. Each WebRTC
//! session uses its selected host's paired TLS identity; no local-display fallback.
use crate::app::RequestClient;
use moonlight_common::{
    high::tokio::MoonlightHost, http::client::async_client::RequestClient as _,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    task::JoinHandle,
    time::{MissedTickBehavior, interval, timeout},
};
use webrtc::data_channel::{DataChannel, DataChannelEvent};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_MESSAGE: usize = 32 * 1024;

/// Cancels clipboard polling immediately when its streaming session ends.
pub struct ClipboardTask(pub JoinHandle<()>);
impl Drop for ClipboardTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Message {
    Read,
    Begin { id: u32, bytes: usize },
    Chunk { id: u32, text: String },
    End { id: u32 },
}

#[derive(Default)]
struct Transfer {
    read_requested: bool,
    pending: Option<(u32, usize, String, Instant)>,
}
impl Transfer {
    fn receive(&mut self, data: &[u8]) -> Result<Option<(u32, String)>, &'static str> {
        if data.len() > MAX_MESSAGE {
            self.pending = None;
            return Err("Clipboard message too large");
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.3.elapsed() > Duration::from_secs(10))
        {
            self.pending = None;
        }
        let message: Message = serde_json::from_slice(data).map_err(|_| {
            self.pending = None;
            "Invalid clipboard message"
        })?;
        match message {
            Message::Read => {
                self.read_requested = true;
            }
            Message::Begin { id, bytes } => {
                self.pending = None;
                if bytes > MAX_BYTES {
                    return Err("Clipboard exceeds 1 MiB");
                }
                self.pending = Some((id, bytes, String::new(), Instant::now()));
            }
            Message::Chunk { id, text } => {
                let Some(p) = self.pending.as_mut() else {
                    return Err("Missing clipboard begin");
                };
                if p.0 != id || p.2.len() + text.len() > p.1 || text.contains('\0') {
                    self.pending = None;
                    return Err("Invalid clipboard chunk");
                }
                p.2.push_str(&text);
            }
            Message::End { id } => {
                let Some(p) = self.pending.take() else {
                    return Err("Missing clipboard begin");
                };
                if p.0 != id || p.1 != p.2.len() {
                    return Err("Incomplete clipboard transfer");
                }
                return Ok(Some((id, p.2)));
            }
        }
        Ok(None)
    }
}

async fn send(channel: &dyn DataChannel, value: serde_json::Value) -> anyhow::Result<()> {
    timeout(Duration::from_secs(4), async {
        while channel.outstanding_bytes().await? > 65536 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        channel.send_text(&value.to_string()).await
    })
    .await??;
    Ok(())
}

async fn send_text(
    channel: &dyn DataChannel,
    id: u32,
    text: &str,
    initial: bool,
) -> anyhow::Result<()> {
    send(
        channel,
        json!({"type":"begin", "id":id, "bytes":text.len(), "initial":initial}),
    )
    .await?;
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + 4096).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        send(
            channel,
            json!({"type":"chunk", "id":id, "text":&text[start..end]}),
        )
        .await?;
        start = end;
    }
    send(channel, json!({"type":"end", "id":id})).await
}

pub async fn run(
    channel: Arc<dyn DataChannel>,
    host: Arc<MoonlightHost<RequestClient>>,
) -> anyhow::Result<()> {
    let (certificate, key, server) = host
        .identity()
        .await
        .ok_or_else(|| anyhow::anyhow!("Host is not paired"))?;
    let client =
        RequestClient::with_certificates(&key.to_pem(), &certificate.to_pem(), &server.to_pem())?;
    let address = host.https_address().await?;
    let mut timer = interval(Duration::from_millis(300));
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut transfer = Transfer::default();
    let mut previous: Option<String> = None;
    let mut sequence = 0u32;
    let mut ready = false;
    let mut reported_error = false;
    let mut initial = true;
    let mut force_read = false;
    loop {
        select! {
            event = channel.poll() => match event {
                Some(DataChannelEvent::OnOpen) => { ready = true; }
                Some(DataChannelEvent::OnMessage(message)) => {
                    ready = true;
                    match transfer.receive(&message.data) {
                        Ok(Some((id, text))) => {
                            let body = json!({"text":text}).to_string();
                            let result = client.clipboard_request(&address, Some(body)).await
                                .ok().and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok());
                            if result.as_ref().and_then(|v| v["ok"].as_bool()) == Some(true) {
                                previous = Some(text);
                                initial = false;
                                send(&*channel, json!({"type":"ack", "id":id})).await?;
                            } else {
                                send(&*channel, json!({"type":"error", "message":"Host clipboard write failed. Use the X11 Sunshine development build."})).await?;
                            }
                        }
                        Ok(None) => {},
                        Err(error) => { send(&*channel, json!({"type":"error", "message":error})).await?; }
                    }
                    if transfer.read_requested {
                        transfer.read_requested = false;
                        force_read = true;
                        timer.reset_immediately();
                    }
                }
                Some(DataChannelEvent::OnClose | DataChannelEvent::OnClosing) | None => break,
                _ => {},
            },
            _ = timer.tick(), if ready => {
                if transfer.pending.as_ref().is_some_and(|p| p.3.elapsed() > Duration::from_secs(10)) {
                    transfer.pending = None;
                    send(&*channel, json!({"type":"error", "message":"Clipboard transfer timed out"})).await?;
                }
                if transfer.pending.is_some() { continue; }
                let result = client.clipboard_request(&address, None).await
                    .ok().and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok());
                if let Some(text) = result.as_ref().and_then(|v| v["text"].as_str()) {
                    if text.len() <= MAX_BYTES && (force_read || previous.as_deref() != Some(text)) {
                        sequence = sequence.wrapping_add(1);
                        send_text(&*channel, sequence, text, initial && !force_read).await?;
                        previous = Some(text.to_owned());
                    }
                    reported_error = false;
                } else if result.as_ref().is_some_and(|v| v.get("text") == Some(&serde_json::Value::Null)) {
                    if initial || previous.take().is_some() || force_read {
                        send(&*channel, json!({"type":"unavailable"})).await?;
                    }
                    reported_error = false;
                } else if !reported_error {
                    send(&*channel, json!({"type":"error", "message":"Host clipboard unavailable. Clipboard sharing requires the X11 Sunshine development build and xclip."})).await?;
                    reported_error = true;
                }
                initial = false;
                force_read = false;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refresh_and_expired_or_malformed_transfers() {
        let mut t = Transfer::default();
        assert!(t.receive(br#"{"type":"read"}"#).unwrap().is_none());
        assert!(t.read_requested);
        t.receive(br#"{"type":"begin","id":1,"bytes":0}"#).unwrap();
        t.pending.as_mut().unwrap().3 = Instant::now() - Duration::from_secs(11);
        assert!(t.receive(br#"{"type":"end","id":1}"#).is_err());
        t.receive(br#"{"type":"begin","id":2,"bytes":0}"#).unwrap();
        assert!(t.receive(b"not json").is_err());
        assert!(t.pending.is_none());
        assert!(t.receive(&vec![b'x'; MAX_MESSAGE + 1]).is_err());
    }
    #[test]
    fn unicode_multiline_and_empty_round_trip() {
        for text in ["", "Příliš žluťoučký 🦎\nline two\t&<>"] {
            let mut t = Transfer::default();
            t.receive(
                json!({"type":"begin","id":7,"bytes":text.len()})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
            for c in text.chars() {
                t.receive(
                    json!({"type":"chunk","id":7,"text":c.to_string()})
                        .to_string()
                        .as_bytes(),
                )
                .unwrap();
            }
            assert_eq!(
                t.receive(br#"{"type":"end","id":7}"#).unwrap(),
                Some((7, text.to_owned()))
            );
        }
    }
    #[test]
    fn rejects_overflow_wrong_id_partial_and_nul() {
        for chunk in [
            r#"{"type":"chunk","id":1,"text":"abcd"}"#,
            r#"{"type":"chunk","id":2,"text":"a"}"#,
            r#"{"type":"chunk","id":1,"text":"\u0000"}"#,
            r#"{"type":"end","id":1}"#,
        ] {
            let mut t = Transfer::default();
            t.receive(br#"{"type":"begin","id":1,"bytes":3}"#).unwrap();
            assert!(t.receive(chunk.as_bytes()).is_err());
            assert!(t.pending.is_none());
        }
        assert!(
            Transfer::default()
                .receive(br#"{"type":"begin","id":1,"bytes":1048577}"#)
                .is_err()
        );
    }
}
