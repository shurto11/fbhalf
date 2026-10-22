//! drmterm(外部モニターの DRM ターミナル)のオーバーレイクライアント。
//! fbrowse/src/drmterm.rs を踏襲。
//!
//! drmterm が DRM マスターを握っている間は fbhalf-drm-output で外部モニターを
//! モードセットできない。代わりに drmterm の中(そのペイン)で起動されたときは、
//! drmterm が用意する画面サイズの共有メモリへ /dev/fb0 の代わりに書き、
//! 書き込み範囲(= tmux ペイン)を RECT で申告して drmterm に重ねてもらう。
//!
//! drmterm は PTY の子へ `DRMTERM_OVERLAY_SOCK` を渡すが、tmux サーバが drmterm
//! より先に起動していると環境変数は届かない。そのため環境変数が無ければ既定の
//! ソケットを探し、drmterm が名乗る tty が本当にこのペインのセッションを表示して
//! いるかを tmux に問い合わせて確かめる(fbterm 側から起動したなら /dev/fb0 へ描く)。
//!
//! プロトコル(行指向・空白区切り)
//!   C→S  HELLO <名前>
//!   S→C  FB <パス> <幅> <高さ> <ストライドpx> <セル幅> <セル高> [tty]
//!   C→S  RECT <x> <y> <w> <h> | RECT none
//!   C→S  FRAME                (共有メモリを更新したので再描画してほしい)

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Info {
    pub fb: String,
    pub width: u32,
    pub height: u32,
    pub stride: u32, // ピクセル単位
    pub cell_w: u32,
    pub cell_h: u32,
    pub tty: String, // drmterm の PTY(tmux クライアントの端末)。不明なら "-"
}

pub struct Overlay {
    sock: UnixStream,
    pub info: Info,
    /// 申告中の矩形。None なら drmterm は何も重ねていない(接続直後も同じ)。
    rect: Option<(u32, u32, u32, u32)>,
    closed: bool,
}

impl Info {
    /// drmterm 上の tmux クライアントの tty(分からなければ None)。
    pub fn client_tty(&self) -> Option<&str> {
        Some(self.tty.as_str()).filter(|t| !t.is_empty() && *t != "-")
    }
}

impl Overlay {
    fn send(&mut self, line: &str) {
        if self.closed {
            return;
        }
        if self.sock.write_all(format!("{line}\n").as_bytes()).is_err() {
            self.closed = true;
        }
    }

    /// 合成してほしい矩形(画面全体のピクセル座標)。None で取り下げ。
    pub fn set_rect(&mut self, r: Option<(u32, u32, u32, u32)>) {
        if r == self.rect {
            return;
        }
        self.rect = r;
        let line = match r {
            Some((x, y, w, h)) => format!("RECT {x} {y} {w} {h}"),
            None => "RECT none".to_string(),
        };
        self.send(&line);
    }

    /// いま drmterm に重ねてもらっているか。
    pub fn shown(&self) -> bool {
        self.rect.is_some()
    }

    /// 共有メモリを書き換えたことを知らせて再描画させる。
    pub fn frame(&mut self) {
        self.send("FRAME");
    }
}

fn tmux(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// このペインの tmux セッション名(tmux 外なら "")。
fn own_session_name() -> String {
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    if pane.is_empty() {
        return String::new();
    }
    tmux(&["display-message", "-p", "-t", &pane, "#{session_name}"]).unwrap_or_default()
}

/// drmterm のクライアント(tty)が、いま session を表示しているか。
pub fn shows_session(tty: &str, session: &str) -> bool {
    if tty.is_empty() || tty == "-" || session.is_empty() {
        return false;
    }
    let Some(out) = tmux(&["list-clients", "-F", "#{client_tty}\t#{client_session}"]) else {
        return false;
    };
    out.lines().any(|l| l.split_once('\t') == Some((tty, session)))
}

/// 探す順: 環境変数 → $XDG_RUNTIME_DIR → /tmp。存在するものだけ返す。
fn socket_candidates() -> Vec<(String, bool)> {
    let mut list: Vec<(String, bool)> = Vec::new();
    if let Ok(p) = std::env::var("DRMTERM_OVERLAY_SOCK") {
        if !p.is_empty() {
            list.push((p, true));
        }
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            list.push((format!("{d}/drmterm-overlay.sock"), false));
        }
    }
    if let Ok(m) = std::fs::metadata("/proc/self") {
        list.push((format!("/tmp/drmterm-overlay-{}.sock", m.uid()), false));
    }
    let mut seen = Vec::new();
    list.retain(|(p, _)| {
        let keep = !seen.contains(p) && Path::new(p).exists();
        seen.push(p.clone());
        keep
    });
    list
}

/// drmterm の中で動いていれば接続して返す。そうでなければ None。
pub fn connect() -> Option<Overlay> {
    for (path, from_env) in socket_candidates() {
        let Some(ov) = try_connect(&path) else { continue };
        // 環境変数で渡された = drmterm の PTY の子。自分で探した場合は、
        // drmterm が本当にこのペインを映しているか確かめてから使う。
        if from_env || shows_session(&ov.info.tty, &own_session_name()) {
            return Some(ov);
        }
    }
    None
}

fn try_connect(path: &str) -> Option<Overlay> {
    let mut sock = UnixStream::connect(path).ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    sock.write_all(b"HELLO fbhalf\n").ok()?;
    let mut line = String::new();
    BufReader::new(sock.try_clone().ok()?).read_line(&mut line).ok()?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.first() != Some(&"FB") || parts.len() < 7 {
        return None;
    }
    let num = |i: usize| parts.get(i).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let info = Info {
        fb: parts[1].to_string(),
        width: num(2),
        height: num(3),
        stride: num(4),
        cell_w: num(5),
        cell_h: num(6),
        tty: parts.get(7).unwrap_or(&"-").to_string(),
    };
    if info.width == 0 || info.height == 0 || info.cell_w == 0 || info.cell_h == 0 {
        return None;
    }
    sock.set_read_timeout(None).ok()?;
    sock.set_write_timeout(Some(Duration::from_millis(200))).ok()?;
    Some(Overlay { sock, info, rect: None, closed: false })
}
