//! fb-server クライアント。task-var/src/fb_client.rs を踏襲。
//!
//! 起動時に自分の名前("fbhalf")と描画領域(rect)を hello として申告し、
//! サーバーから届く {"visible":bool, "clip":[...]} を呼び出し側へ渡す。
//! `clip` は描画禁止矩形(自分より優先度が高い表示中クライアントとの重なり
//! 領域)。呼び出し側はその矩形を避けて描画する。
//! 描画領域が変わったら共有 rect を更新すると、次のポーリングで {"rect":...}
//! をサーバーへ送り直す(auto 追従で領域が動くケース用)。
//! (Hello 送信 → set_read_timeout → chunk 読み → \n 区切り JSON → 切断で張り直し)

use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// フレームバッファ上の矩形(ピクセル座標、左上原点)。protocol.rs と同形。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// サーバーから届く可視性通知(呼び出し側へ渡す形)。
#[derive(Debug, Clone)]
pub struct VisMsg {
    pub visible: bool,
    pub clip: Vec<Rect>,
}

/// 接続直後に1行だけ送る申告メッセージ。
#[derive(Serialize)]
struct Hello {
    hello: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    rect: Option<Rect>,
}

/// 描画領域が動いたときに送る更新メッセージ。
#[derive(Serialize)]
struct RectUpdate {
    rect: Option<Rect>,
}

/// サーバーから届く可視性通知(受信用)。
#[derive(Deserialize)]
struct RawVisible {
    visible: bool,
    #[serde(default)]
    clip: Vec<Rect>,
}

/// ソケットパス: `$FB_SERVER_SOCK` > `$XDG_RUNTIME_DIR/fb-server.sock` > `/tmp/...`。
fn socket_path() -> String {
    if let Ok(p) = std::env::var("FB_SERVER_SOCK") {
        if !p.is_empty() {
            return p;
        }
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() => format!("{d}/fb-server.sock"),
        _ => "/tmp/fb-server.sock".to_string(),
    }
}

/// fb-client スレッドを起動する(detached)。切断・接続失敗時は再接続し続ける。
/// `rect` は現在の描画領域(共有)。呼び出し側が更新すると次のポーリングで
/// サーバーへ申告し直す。fb-server が未起動の間は既定で visible=true のまま。
pub fn spawn(name: &'static str, rect: Arc<Mutex<Option<Rect>>>, tx: Sender<VisMsg>) {
    std::thread::spawn(move || loop {
        if let Err(e) = session(name, &rect, &tx) {
            eprintln!("fbhalf: fb-server 接続待ち ({e})");
        }
        std::thread::sleep(Duration::from_millis(500));
    });
}

/// 1接続ぶんの受信ループ。EOF / エラーで戻る(呼び出し側が張り直す)。
fn session(
    name: &'static str,
    rect: &Arc<Mutex<Option<Rect>>>,
    tx: &Sender<VisMsg>,
) -> std::io::Result<()> {
    let stream = UnixStream::connect(socket_path())?;
    let mut cur = *rect.lock().unwrap();
    let hello = Hello { hello: name, rect: cur };
    let line = serde_json::to_string(&hello).unwrap_or_default();
    (&stream).write_all(format!("{line}\n").as_bytes())?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        // 描画領域が変わっていたらサーバーへ申告し直す。
        let now = *rect.lock().unwrap();
        if now != cur {
            cur = now;
            let upd = serde_json::to_string(&RectUpdate { rect: now }).unwrap_or_default();
            (&stream).write_all(format!("{upd}\n").as_bytes())?;
        }
        match (&stream).read(&mut chunk) {
            Ok(0) => return Ok(()), // サーバー切断
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let l: Vec<u8> = buf.drain(..=pos).collect();
                    if let Ok(v) = serde_json::from_slice::<RawVisible>(&l) {
                        let msg = VisMsg { visible: v.visible, clip: v.clip };
                        if tx.send(msg).is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }
    }
}
