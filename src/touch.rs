//! touch-server クライアント。
//!
//! fbhalf が描画している領域(`DrawRegion`)を画面に対する 0..1 の矩形として
//! touch-server に申告し、その矩形内で起きたタッチイベントだけを受け取る
//! (dopagaki / spotatui-pip と同じ region 方式)。届いた down/move/up を
//! ジェスチャとして解釈し、[`TouchAction`] に変換してメインループへ mpsc で送る。
//!
//! ジェスチャの対応:
//!   - 2本指ピンチイン/アウト → 拡大縮小        ([`TouchAction::ZoomFactor`])
//!   - 1本指の左右スクロール   → ページ移動      ([`TouchAction::NextPage`] / [`TouchAction::PrevPage`])
//!   - 1本指の上下スクロール   → 画面スクロール(パン) ([`TouchAction::PanBy`])
//!
//! サーバー(`touch-server`)が起動していない場合は接続に失敗するだけで、
//! 静かにリトライし続ける(fbhalf のキーボード操作には影響しない)。

use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::DrawRegion;

/// 1本指ドラッグで軸(上下/左右)を確定するしきい値(画面に対する割合)。
/// これ未満の微小な移動ではパン/ページ判定を始めない(手ブレ対策)。
const LOCK_FRAC: f64 = 0.02;

/// 左右スワイプをページ移動と判定する最小の横移動量(画面幅に対する割合)。
const PAGE_SWIPE_FRAC: f64 = 0.12;

/// メインループへ渡すタッチ由来の操作。
pub enum TouchAction {
    /// 現在の zoom を factor 倍する(ピンチ)。
    ZoomFactor(f32),
    /// framebuffer ピクセル単位でパンする(上下スクロール)。(dx, dy)。
    PanBy(i32, i32),
    /// 次のページへ。
    NextPage,
    /// 前のページへ。
    PrevPage,
}

/// 画面に対する 0..1 の割合で表した矩形(touch-server の FracRect に対応)。
#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
struct FracRect {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

/// 接続直後に 1 行だけ送る申告メッセージ。
#[derive(Serialize)]
struct Hello {
    hello: &'static str,
    pane: Option<String>,
    region: Option<FracRect>,
}

/// サーバーから届くイベント。必要なフィールドだけを緩く受ける
/// (余分なフィールド ctx / fx / fy などは無視)。
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum RawEvent {
    /// 接地。ジェスチャ状態のリセットにだけ使う。
    Down,
    /// ドラッグ中の移動。dx/dy は直前レポートからの移動量(frac, 下/右が正)。
    /// n は接地中の指の本数、spread は接地点間の最大距離(frac)。
    Move {
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
        #[serde(default = "one")]
        n: usize,
        #[serde(default)]
        spread: f64,
    },
    /// 離した瞬間。開始(fx0,fy0)〜終了(fx1,fy1)の frac。
    Up {
        #[serde(default)]
        fx0: f64,
        #[serde(default)]
        fy0: f64,
        #[serde(default)]
        fx1: f64,
        #[serde(default)]
        fy1: f64,
    },
}

/// `n`(指の本数)のデシリアライズ既定値。古いメッセージは 1 本指扱い。
fn one() -> usize {
    1
}

/// 1本指ドラッグでロックした軸。
#[derive(Clone, Copy, PartialEq)]
enum Axis {
    Undecided,
    Horizontal,
    Vertical,
}

/// 1 ジェスチャ(down〜up)ぶんの途中状態。
struct Gesture {
    axis: Axis,
    /// down からの累積移動量(frac)。軸ロック判定に使う。
    acc_dx: f64,
    acc_dy: f64,
    /// 直前の move で観測した spread(ピンチ倍率の基準)。0 はピンチ非アクティブ。
    prev_spread: f64,
}

impl Default for Gesture {
    fn default() -> Self {
        Gesture { axis: Axis::Undecided, acc_dx: 0.0, acc_dy: 0.0, prev_spread: 0.0 }
    }
}

/// ソケットパス: `$TOUCH_SERVER_SOCK` > `$XDG_RUNTIME_DIR/touch-server.sock` > `/tmp/...`。
fn socket_path() -> String {
    if let Ok(p) = std::env::var("TOUCH_SERVER_SOCK") {
        if !p.is_empty() {
            return p;
        }
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() => format!("{d}/touch-server.sock"),
        _ => "/tmp/touch-server.sock".to_string(),
    }
}

/// `DrawRegion`(ピクセル)を画面に対する frac 矩形へ変換する。
fn region_frac(reg: DrawRegion, fb_w: f64, fb_h: f64) -> FracRect {
    FracRect {
        left: reg.x as f64 / fb_w,
        top: reg.y as f64 / fb_h,
        right: (reg.x + reg.w) as f64 / fb_w,
        bottom: (reg.y + reg.h) as f64 / fb_h,
    }
}

/// touch-client スレッドを起動し、メインループが受け取る Receiver を返す。
/// スレッドはプロセス終了時に道連れで止まる。
///
/// `shared` は現在の描画領域(auto モードで変化する)。変化を検知すると
/// 新しい region で申告し直す。`fb_w`/`fb_h` は画面(fb)のピクセル寸法。
pub fn spawn(shared: Arc<Mutex<DrawRegion>>, fb_w: f64, fb_h: f64) -> Receiver<TouchAction> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        // 接続失敗・切断・領域変更で抜けたら静かに張り直す
        let _ = session(&shared, &tx, fb_w, fb_h);
        std::thread::sleep(Duration::from_millis(500));
    });
    rx
}

/// 1 接続ぶんの受信ループ。EOF か描画領域の変更で Ok を返して張り直しさせる。
fn session(
    shared: &Arc<Mutex<DrawRegion>>,
    tx: &Sender<TouchAction>,
    fb_w: f64,
    fb_h: f64,
) -> std::io::Result<()> {
    let declared = *shared.lock().unwrap();
    let region = region_frac(declared, fb_w, fb_h);

    let stream = UnixStream::connect(socket_path())?;
    let hello = Hello {
        hello: "fbhalf",
        pane: std::env::var("TMUX_PANE").ok(),
        region: Some(region),
    };
    let line = serde_json::to_string(&hello).unwrap_or_default();
    (&stream).write_all(format!("{line}\n").as_bytes())?;
    // 領域変更を検知するため、ブロックしっぱなしにせず定期的に起こす
    stream.set_read_timeout(Some(Duration::from_millis(400)))?;

    let mut g = Gesture::default();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match (&stream).read(&mut chunk) {
            Ok(0) => return Ok(()), // サーバー切断
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    if let Ok(ev) = serde_json::from_slice::<RawEvent>(&line) {
                        handle_event(ev, &mut g, tx, fb_h);
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                // 描画領域が変わっていたら、新しい region で申告し直す
                if *shared.lock().unwrap() != declared {
                    return Ok(());
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// `FBHALF_TOUCH_LOG` が指すファイルに 1 行追記する(切り分け用デバッグ)。
/// 設定が無ければ何もしない。fbhalf の描画を汚さないよう stderr ではなくファイルへ。
fn debug_log(msg: &str) {
    if let Ok(path) = std::env::var("FBHALF_TOUCH_LOG") {
        if !path.is_empty() {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{msg}");
            }
        }
    }
}

/// 1 イベントをジェスチャとして解釈し、対応する [`TouchAction`] を送る。
fn handle_event(ev: RawEvent, g: &mut Gesture, tx: &Sender<TouchAction>, fb_h: f64) {
    match &ev {
        RawEvent::Down => debug_log("down"),
        RawEvent::Move { dx, dy, n, spread } => {
            debug_log(&format!("move n={n} spread={spread:.4} dx={dx:.4} dy={dy:.4}"))
        }
        RawEvent::Up { fx0, fy0, fx1, fy1 } => {
            debug_log(&format!("up d=({:.3},{:.3})", fx1 - fx0, fy1 - fy0))
        }
    }
    match ev {
        RawEvent::Down => {
            *g = Gesture::default();
        }

        // 2本指 → ピンチ(拡大縮小)。1本指 → 上下パン/左右ページの軸判定。
        RawEvent::Move { dx, dy, n, spread } => {
            if n >= 2 {
                // ピンチ: 直前の spread との比を zoom 倍率として送る
                if spread > 0.0 {
                    if g.prev_spread > 0.0 {
                        let factor = (spread / g.prev_spread) as f32;
                        if factor.is_finite() && factor > 0.0 {
                            let _ = tx.send(TouchAction::ZoomFactor(factor));
                        }
                    }
                    g.prev_spread = spread;
                }
                // ピンチ中は 1本指のパン軸をリセット(指を離すまで混ざらない)
                g.axis = Axis::Undecided;
                g.acc_dx = 0.0;
                g.acc_dy = 0.0;
            } else {
                g.prev_spread = 0.0; // ピンチ解除
                g.acc_dx += dx;
                g.acc_dy += dy;
                if g.axis == Axis::Undecided
                    && g.acc_dx.abs().max(g.acc_dy.abs()) > LOCK_FRAC
                {
                    g.axis = if g.acc_dy.abs() >= g.acc_dx.abs() {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    };
                }
                // 上下スクロール → パン(content-follows-finger: 指を下げると上が見える)
                if g.axis == Axis::Vertical {
                    let px = (-dy * fb_h) as i32;
                    if px != 0 {
                        let _ = tx.send(TouchAction::PanBy(0, px));
                    }
                }
            }
        }

        // 離した瞬間の総移動量で左右スワイプ(ページ移動)を判定
        RawEvent::Up { fx0, fy0, fx1, fy1 } => {
            let dx = fx1 - fx0;
            let dy = fy1 - fy0;
            if dx.abs() >= PAGE_SWIPE_FRAC && dx.abs() > dy.abs() {
                // 左スワイプ(dx<0) → 次ページ / 右スワイプ → 前ページ
                let _ = tx.send(if dx < 0.0 {
                    TouchAction::NextPage
                } else {
                    TouchAction::PrevPage
                });
            }
            *g = Gesture::default();
        }
    }
}
