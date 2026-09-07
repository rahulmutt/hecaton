//! The daemon's end of `GET /v1/plugin-host/agents/{id}/attach` (plugins
//! spec §18.4): one WebSocket bridged to the runner's `PtyStream`. Binary
//! frames are terminal bytes both ways; the one text frame is a resize.
//! Dropping the stream at the end is what ends the terminal session.

use std::io::{Read, Write};

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use hecaton_api::ResizeFrame;
use hecaton_core::PtyStream;
use tokio::sync::mpsc;

const READ_CHUNK: usize = 8192;
/// The window closed, or the peer is done.
pub const CLOSE_NORMAL: u16 = 1000;
/// A text frame that is not a resize.
pub const CLOSE_UNSUPPORTED: u16 = 1003;
/// The runner's side failed.
pub const CLOSE_ERROR: u16 = 1011;

pub async fn bridge(mut socket: WebSocket, stream: Box<dyn PtyStream>) {
    let (reader, mut writer) = match (stream.reader(), stream.writer()) {
        (Ok(r), Ok(w)) => (r, w),
        (Err(e), _) | (_, Err(e)) => {
            close(&mut socket, CLOSE_ERROR, &format!("attach: {e}")).await;
            return;
        }
    };
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    // The reader blocks on the PTY; it lives on a blocking thread and ends
    // when the stream is dropped below (the read fails once the client
    // process is gone) or the socket is.
    let pump = tokio::task::spawn_blocking(move || pump_reader(reader, &tx));
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(bytes) => {
                    if socket.send(Message::Binary(Bytes::from(bytes))).await.is_err() {
                        break;
                    }
                }
                None => {
                    close(&mut socket, CLOSE_NORMAL, "the window closed").await;
                    break;
                }
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                        close(&mut socket, CLOSE_ERROR, "the window closed").await;
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => match ResizeFrame::parse(text.as_str()) {
                    Some(frame) => {
                        if let Err(e) = stream.resize(frame.resize.cols, frame.resize.rows) {
                            tracing::debug!("attach resize failed: {e}");
                        }
                    }
                    None => {
                        close(&mut socket, CLOSE_UNSUPPORTED, "expected a resize frame").await;
                        break;
                    }
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    // Ending the session is synchronous — the tmux stream's drop kills its
    // client and waits for it — so it happens off the async runtime, like
    // the reader. Awaited so the window is really gone before the handler
    // returns; the reader ends by itself once the stream is closed.
    drop(pump);
    let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
}

fn pump_reader(mut reader: Box<dyn Read + Send>, tx: &mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        }
    }
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}
