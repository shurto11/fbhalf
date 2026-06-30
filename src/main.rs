use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

// ── 外部ディスプレイ (DRM) 出力 ───────────────────────────────────────────────

const DRM_DEFAULT_CONNECTOR: &str = "DP-1";
const DRM_DEFAULT_DEVICE: &str = "/dev/dri/card1";
const DRM_SHM_PATH: &str = "/tmp/fbhalf-ext";
const DRM_HELPER_NAME: &str = "fbhalf-drm-output";

/// fbhalf-drm-output ヘルパーの実行ファイルを探す
/// （fbhalf 自身と同じディレクトリ → PATH の順）
fn find_drm_output_helper() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(DRM_HELPER_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(DRM_HELPER_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// drm-output が未起動なら sudo 経由で起動し、共有メモリの info ファイルが
/// 出現するまで待つ
fn ensure_drm_output_running(connector: &str) -> Result<(), Box<dyn std::error::Error>> {
    let info_path = format!("{}.info", DRM_SHM_PATH);
    if std::path::Path::new(&info_path).exists() {
        return Ok(());
    }

    let helper = find_drm_output_helper().ok_or_else(|| {
        format!(
            "{} が見つかりません。`make` でビルドし、fbhalf と同じディレクトリか PATH に配置してください",
            DRM_HELPER_NAME,
        )
    })?;

    eprintln!(
        "外部ディスプレイ出力を起動しています: sudo {} {} {} {}",
        helper.display(), connector, DRM_SHM_PATH, DRM_DEFAULT_DEVICE,
    );
    std::process::Command::new("sudo")
        .arg(&helper)
        .arg(connector)
        .arg(DRM_SHM_PATH)
        .arg(DRM_DEFAULT_DEVICE)
        .spawn()?;

    let start = Instant::now();
    while !std::path::Path::new(&info_path).exists() {
        if start.elapsed() > Duration::from_secs(10) {
            return Err("drm-output の起動待ちでタイムアウトしました".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// 外部ディスプレイへの出力を準備し、(FbInfo, 書き込み先パス) を返す
fn setup_display_output(connector: &str) -> Result<(FbInfo, String), Box<dyn std::error::Error>> {
    ensure_drm_output_running(connector)?;

    let info_path = format!("{}.info", DRM_SHM_PATH);
    let info = std::fs::read_to_string(&info_path)?;
    let nums: Vec<u32> = info
        .trim()
        .split_whitespace()
        .take(3)
        .map(|v| v.parse())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("info ファイルの形式が不正です: {}", info_path))?;
    if nums.len() < 3 {
        return Err(format!("info ファイルの形式が不正です: {}", info_path).into());
    }

    Ok((
        FbInfo { width: nums[0], height: nums[1], bpp: 32, stride: nums[2] },
        DRM_SHM_PATH.to_string(),
    ))
}

// ── フレームバッファ情報 ──────────────────────────────────────────────────────

struct FbInfo {
    width: u32,
    height: u32,
    bpp: u32,
    stride: u32,
}

impl FbInfo {
    fn read() -> Result<Self, Box<dyn std::error::Error>> {
        let vsize = std::fs::read_to_string("/sys/class/graphics/fb0/virtual_size")?;
        let (w, h) = vsize
            .trim()
            .split_once(',')
            .ok_or_else(|| format!("bad virtual_size: {}", vsize.trim()))?;
        Ok(FbInfo {
            width: w.parse()?,
            height: h.parse()?,
            bpp: std::fs::read_to_string("/sys/class/graphics/fb0/bits_per_pixel")?
                .trim()
                .parse()?,
            stride: std::fs::read_to_string("/sys/class/graphics/fb0/stride")?
                .trim()
                .parse()?,
        })
    }
}

// ── 描画領域（ピクセル座標） ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
struct DrawRegion {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

fn parse_region(spec: &str, fb: &FbInfo) -> Option<DrawRegion> {
    let fw = fb.width;
    let fh = fb.height;
    let hw = fw / 2;
    let hh = fh / 2;
    match spec {
        "full"                => Some(DrawRegion { x: 0,  y: 0,  w: fw, h: fh }),
        "left"                => Some(DrawRegion { x: 0,  y: 0,  w: hw, h: fh }),
        "right"               => Some(DrawRegion { x: hw, y: 0,  w: hw, h: fh }),
        "topleft"     | "tl"  => Some(DrawRegion { x: 0,  y: 0,  w: hw, h: hh }),
        "topright"    | "tr"  => Some(DrawRegion { x: hw, y: 0,  w: hw, h: hh }),
        "bottomleft"  | "bl"  => Some(DrawRegion { x: 0,  y: hh, w: hw, h: hh }),
        "bottomright" | "br"  => Some(DrawRegion { x: hw, y: hh, w: hw, h: hh }),
        "auto"                => tmux_info(fb, &std::env::var("TMUX_PANE").unwrap_or_default()).map(|i| i.reg),
        _                     => None,
    }
}

struct TmuxInfo {
    window_active: bool,
    reg: DrawRegion,
}

/// tmux のペイン情報を取得する（ウィンドウアクティブ状態 + ピクセル座標）
fn tmux_info(fb: &FbInfo, pane_id: &str) -> Option<TmuxInfo> {
    // $TMUX が未設定でも tmux サーバーが動いていれば検出できるよう、
    // env var ガードを外して終了コードで判断する
    let fmt = "#{window_active},#{pane_left},#{pane_top},#{pane_width},#{pane_height},#{window_width},#{window_height}";
    let mut cmd = std::process::Command::new("tmux");
    cmd.arg("display-message").arg("-p");
    if !pane_id.is_empty() {
        cmd.arg("-t").arg(pane_id);
    }
    cmd.arg(fmt);

    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let nums: Vec<u32> = s.trim().split(',')
        .map(|v| v.trim().parse::<u32>().ok())
        .collect::<Option<Vec<u32>>>()?;

    if nums.len() < 7 {
        return None;
    }
    let (active, pane_left, pane_top, pane_w, pane_h, win_w, win_h) =
        (nums[0], nums[1], nums[2], nums[3], nums[4], nums[5], nums[6]);

    if win_w == 0 || win_h == 0 {
        return None;
    }
    let cell_w = fb.width  / win_w;
    let cell_h = fb.height / win_h;

    Some(TmuxInfo {
        window_active: active == 1,
        reg: DrawRegion {
            x: pane_left * cell_w,
            y: pane_top  * cell_h,
            w: pane_w    * cell_w,
            h: pane_h    * cell_h,
        },
    })
}

// ── PNG ローダー ──────────────────────────────────────────────────────────────

fn load_image(path: &str) -> Result<(Vec<u8>, u32, u32), Box<dyn std::error::Error>> {
    let reader = match image::io::Reader::open(path) {
        Ok(r) => r,
        Err(e) => return Err(format!("ファイルを開けません: {}: {}", path, e).into()),
    };
    let reader = match reader.with_guessed_format() {
        Ok(r) => r,
        Err(e) => return Err(format!("画像フォーマットの判定に失敗しました: {}: {}", path, e).into()),
    };
    let img = match reader.decode() {
        Ok(i) => i,
        Err(e) => return Err(format!("画像のデコードに失敗しました: {}: {}", path, e).into()),
    };
    let rgba_img = img.to_rgba8();
    let (w, h) = (rgba_img.width(), rgba_img.height());
    Ok((rgba_img.into_raw(), w, h))
}

/// ファイルが image クレートで判別可能な画像フォーマットかを簡易チェックする
fn is_image_supported(path: &str) -> bool {
    image::io::Reader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map(|r| r.format().is_some())
        .unwrap_or(false)
}

// ── スケーリング・描画 ────────────────────────────────────────────────────────

fn scale_nearest(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if dst_w == 0 || dst_h == 0 {
        return vec![];
    }
    let mut dst = vec![0u8; (dst_w * dst_h * 4) as usize];
    let xr = src_w as f32 / dst_w as f32;
    let yr = src_h as f32 / dst_h as f32;
    for dy in 0..dst_h {
        for dx in 0..dst_w {
            let sx = ((dx as f32 * xr) as u32).min(src_w - 1);
            let sy = ((dy as f32 * yr) as u32).min(src_h - 1);
            let s = ((sy * src_w + sx) * 4) as usize;
            let d = ((dy * dst_w + dx) * 4) as usize;
            dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    dst
}

fn clear_region(
    fb: &mut std::fs::File,
    fb_info: &FbInfo,
    reg: DrawRegion,
) -> Result<(), Box<dyn std::error::Error>> {
    let bpp = (fb_info.bpp / 8) as usize;
    let black = vec![0u8; reg.w as usize * bpp];
    for row in 0..reg.h {
        let off = (reg.y + row) as u64 * fb_info.stride as u64 + reg.x as u64 * bpp as u64;
        fb.seek(SeekFrom::Start(off))?;
        fb.write_all(&black)?;
    }
    Ok(())
}

fn blit(
    fb: &mut std::fs::File,
    image: &[u8],
    img_w: u32,
    img_h: u32,
    fb_info: &FbInfo,
    reg: DrawRegion,
    dst_x: i32,
    dst_y: i32,
    src_x: i32,
    src_y: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let bpp = (fb_info.bpp / 8) as usize;
    let mut row_buf = vec![0u8; reg.w as usize * bpp];

    for fb_row in 0..reg.h as i32 {
        let sy = fb_row - dst_y + src_y;
        if sy < 0 || sy >= img_h as i32 {
            continue;
        }
        row_buf.fill(0);
        for fb_col in 0..reg.w as i32 {
            let sx = fb_col - dst_x + src_x;
            if sx < 0 || sx >= img_w as i32 {
                continue;
            }
            let s = ((sy as u32 * img_w + sx as u32) * 4) as usize;
            let r = image[s];
            let g = image[s + 1];
            let b = image[s + 2];
            let d = fb_col as usize * bpp;
            match fb_info.bpp {
                32 => { row_buf[d] = b; row_buf[d+1] = g; row_buf[d+2] = r; row_buf[d+3] = 0; }
                24 => { row_buf[d] = b; row_buf[d+1] = g; row_buf[d+2] = r; }
                16 => {
                    let p: u16 = ((r as u16 & 0xf8) << 8)
                        | ((g as u16 & 0xfc) << 3)
                        | (b as u16 >> 3);
                    row_buf[d..d+2].copy_from_slice(&p.to_le_bytes());
                }
                _ => {}
            }
        }
        let off = (reg.y as i32 + fb_row) as u64 * fb_info.stride as u64
            + reg.x as u64 * bpp as u64;
        fb.seek(SeekFrom::Start(off))?;
        fb.write_all(&row_buf)?;
    }
    Ok(())
}

// ── ビューア ─────────────────────────────────────────────────────────────────

struct Viewer {
    files: Vec<String>,
    idx: usize,
    zoom: f32,
    pan_x: i32,
    pan_y: i32,
    reg: DrawRegion,
    fb_info: FbInfo,
    cache: Option<(String, Vec<u8>, u32, u32)>,
    auto_track: bool,
    /// $TMUX_PANE の値（例: "%3"）。auto モード時に自ペインを特定するために使う。
    pane_id: String,
    /// 描画後に保存したフレームバッファの先頭部分。fbterm上書き検出に使う。
    sentinel: Vec<u8>,
}

impl Viewer {
    const ZOOM_STEP: f32 = 0.25;
    const PAN_STEP: i32 = 80;

    fn current_path(&self) -> &str {
        &self.files[self.idx]
    }

    fn load_current(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.current_path().to_owned();
        if self.cache.as_ref().map(|(p, ..)| p.as_str()) == Some(&path) {
            return Ok(());
        }
        let (rgba, w, h) = load_image(&path)?;
        self.cache = Some((path, rgba, w, h));
        Ok(())
    }

    fn render(&mut self, fb: &mut std::fs::File) -> Result<(), Box<dyn std::error::Error>> {
        self.load_current()?;
        let (_, rgba, img_w, img_h) = self.cache.as_ref().unwrap();
        let (img_w, img_h) = (*img_w, *img_h);
        let reg = self.reg;

        let fit_scale = (reg.w as f32 / img_w as f32).min(reg.h as f32 / img_h as f32);
        let total_scale = fit_scale * self.zoom;

        let scaled_w = ((img_w as f32 * total_scale).round() as u32).max(1);
        let scaled_h = ((img_h as f32 * total_scale).round() as u32).max(1);
        let scaled = scale_nearest(rgba, img_w, img_h, scaled_w, scaled_h);

        let dst_x = ((reg.w as i32 - scaled_w as i32) / 2).max(0);
        let dst_y = ((reg.h as i32 - scaled_h as i32) / 2).max(0);

        self.pan_x = self.pan_x.clamp(0, (scaled_w as i32 - reg.w as i32).max(0));
        self.pan_y = self.pan_y.clamp(0, (scaled_h as i32 - reg.h as i32).max(0));

        let src_x = if scaled_w > reg.w { self.pan_x } else { 0 };
        let src_y = if scaled_h > reg.h { self.pan_y } else { 0 };

        blit(fb, &scaled, scaled_w, scaled_h, &self.fb_info, reg, dst_x, dst_y, src_x, src_y)?;
        fb.flush()?;
        self.sentinel = read_sentinel(fb, &self.fb_info, reg);

        if !self.auto_track {
            eprint!(
                "\r[{}/{}] {}  zoom:{:.0}%  ",
                self.idx + 1, self.files.len(), self.current_path(), self.zoom * 100.0,
            );
        }
        Ok(())
    }

    fn next(&mut self) { if self.idx + 1 < self.files.len() { self.idx += 1; self.pan_x = 0; self.pan_y = 0; } }
    fn prev(&mut self) { if self.idx > 0 { self.idx -= 1; self.pan_x = 0; self.pan_y = 0; } }
    fn zoom_in(&mut self)  { self.zoom = (self.zoom + Self::ZOOM_STEP).min(8.0); }
    fn zoom_out(&mut self) { self.zoom = (self.zoom - Self::ZOOM_STEP).max(0.1); }
    fn reset_view(&mut self) { self.zoom = 1.0; self.pan_x = 0; self.pan_y = 0; }
}

// ── エントリポイント ──────────────────────────────────────────────────────────

fn usage() -> ! {
    eprintln!("使い方: fbhalf <region> <file.png|file.jpg> [file2.png ...]");
    eprintln!();
    eprintln!("  region: full  … 画面全体");
    eprintln!("          left, right");
    eprintln!("          topleft/tl, topright/tr, bottomleft/bl, bottomright/br");
    eprintln!("          auto  … tmux ペイン位置を自動検出");
    eprintln!();
    eprintln!("  --display[=CONNECTOR] : 外部ディスプレイに DRM 出力 (デフォルト: {})", DRM_DEFAULT_CONNECTOR);
    eprintln!();
    eprintln!("  n/Space      : 次    p/b/BS: 前");
    eprintln!("  +/=          : ズームイン   -: ズームアウト   0: リセット");
    eprintln!("  hjkl/矢印    : パン");
    eprintln!("  q/ESC        : 終了");
    std::process::exit(1);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().collect();

    // --display[=CONNECTOR] オプションを取り出す（外部ディスプレイへ DRM 出力）
    let display_connector: Option<String> = if let Some(pos) = args
        .iter()
        .position(|a| a == "--display" || a.starts_with("--display="))
    {
        let arg = args.remove(pos);
        Some(match arg.split_once('=') {
            Some((_, c)) if !c.is_empty() => c.to_string(),
            _ => DRM_DEFAULT_CONNECTOR.to_string(),
        })
    } else {
        None
    };

    let (fb_info, fb_path): (FbInfo, String) = match &display_connector {
        Some(connector) => setup_display_output(connector)?,
        None => (FbInfo::read()?, "/dev/fb0".to_string()),
    };

    // 引数なし → カレントディレクトリの *.{png,jpg,jpeg} を auto で開く
    let (region_str, mut files): (&str, Vec<String>) = if args.len() == 1 {
        let mut imgs: Vec<String> = std::fs::read_dir(".")
            .unwrap_or_else(|_| usage())
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| { let l = n.to_lowercase(); l.ends_with(".png") || l.ends_with(".jpg") || l.ends_with(".jpeg") })
            .collect();
        imgs.sort();
        if imgs.is_empty() {
            eprintln!("画像ファイルが見つかりません");
            usage();
        }
        ("auto", imgs)
    } else if args.len() >= 2 && parse_region(&args[1], &fb_info).is_some() {
        if args.len() < 3 { usage(); }
        (&args[1], args[2..].to_vec())
    } else {
        ("auto", args[1..].to_vec())
    };

    // 読み込み可能な画像フォーマットではないファイルを除外しておく
    files.retain(|p| is_image_supported(p));
    if files.is_empty() {
        eprintln!("対応する画像ファイルが見つかりません");
        usage();
    }

    let auto_track = region_str == "auto";
    let pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
    let reg = parse_region(region_str, &fb_info).unwrap_or_else(|| {
        eprintln!("不明なregion: {}", region_str);
        usage();
    });

    if !auto_track {
        let target = match &display_connector {
            Some(connector) => format!("外部ディスプレイ ({})", connector),
            None => "フレームバッファ".to_string(),
        };
        eprintln!(
            "{}: {}x{} {}bpp  描画領域: ({},{}) {}x{}px",
            target, fb_info.width, fb_info.height, fb_info.bpp,
            reg.x, reg.y, reg.w, reg.h,
        );
    }

    let mut viewer = Viewer {
        files,
        idx: 0,
        zoom: 1.0,
        pan_x: 0,
        pan_y: 0,
        reg,
        fb_info,
        cache: None,
        auto_track,
        pane_id,
        sentinel: Vec::new(),
    };

    let mut fb = std::fs::OpenOptions::new().read(true).write(true).open(&fb_path)?;

    viewer.render(&mut fb)?;

    enable_raw_mode()?;
    let result = run_loop(&mut viewer, &mut fb);
    disable_raw_mode()?;

    let _ = clear_region(&mut fb, &viewer.fb_info, viewer.reg);
    if !viewer.auto_track {
        eprintln!();
    }

    result
}

/// 領域の先頭64ピクセル分を読み取ってセンチネルとして返す
fn read_sentinel(fb: &mut std::fs::File, fb_info: &FbInfo, reg: DrawRegion) -> Vec<u8> {
    let bpp = (fb_info.bpp / 8) as usize;
    let size = (reg.w as usize).min(64) * bpp;
    let offset = reg.y as u64 * fb_info.stride as u64 + reg.x as u64 * bpp as u64;
    let mut buf = vec![0u8; size];
    let _ = fb.seek(SeekFrom::Start(offset));
    let _ = fb.read_exact(&mut buf);
    buf
}

fn execute_command(
    cmd: &str,
    viewer: &mut Viewer,
    fb: &mut std::fs::File,
) -> Result<bool, Box<dyn std::error::Error>> {
    let cmd = cmd.trim();
    // :p → 現在のページ番号を表示
    if cmd == "p" {
        use std::io::Write;
        print!(
            "\r[{}/{}] {}  ",
            viewer.idx + 1,
            viewer.files.len(),
            viewer.current_path(),
        );
        let _ = std::io::stdout().flush();
        return Ok(true);
    }

    // :N → N ページ目へジャンプ（1始まり）
    if let Ok(n) = cmd.parse::<usize>() {
        if n >= 1 && n <= viewer.files.len() {
            viewer.idx = n - 1;
            viewer.render(fb)?;
            return Ok(true);
        }
        return Ok(false);
    }
    Ok(false)
}

fn cmd_prompt_show(buf: &str) {
    use std::io::Write;
    print!("\r:{buf}  ");
    let _ = std::io::stdout().flush();
}

fn cmd_prompt_clear() {
    use std::io::Write;
    print!("\r                        \r");
    let _ = std::io::stdout().flush();
}

fn run_loop(
    viewer: &mut Viewer,
    fb: &mut std::fs::File,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_tmux_check = Instant::now();
    let tmux_interval = Duration::from_millis(500);
    let mut cmd_mode = false;
    let mut cmd_buf = String::new();

    loop {
        // tmux ペイン追跡（auto モード時のみ）
        if viewer.auto_track && last_tmux_check.elapsed() >= tmux_interval {
            last_tmux_check = Instant::now();
            if let Some(info) = tmux_info(&viewer.fb_info, &viewer.pane_id) {
                // 別ウィンドウに切り替え中は描画しない
                if !info.window_active {
                    continue;
                }
                let region_changed = info.reg != viewer.reg;
                let overwritten = !viewer.sentinel.is_empty()
                    && read_sentinel(fb, &viewer.fb_info, viewer.reg) != viewer.sentinel;

                if region_changed || overwritten {
                    if region_changed {
                        clear_region(fb, &viewer.fb_info, viewer.reg)?;
                        viewer.reg = info.reg;
                    }
                    viewer.render(fb)?;
                }
            }
        }

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(KeyEvent { code, modifiers, .. }) = event::read()? else {
            continue;
        };

        if cmd_mode {
            match code {
                KeyCode::Esc => {
                    cmd_mode = false;
                    cmd_buf.clear();
                    cmd_prompt_clear();
                }
                KeyCode::Enter => {
                    let keep = cmd_buf == "p";
                    execute_command(&cmd_buf, viewer, fb)?;
                    cmd_mode = false;
                    cmd_buf.clear();
                    if !keep {
                        cmd_prompt_clear();
                    }
                }
                KeyCode::Backspace => {
                    cmd_buf.pop();
                    if cmd_buf.is_empty() {
                        cmd_mode = false;
                        cmd_prompt_clear();
                    } else {
                        cmd_prompt_show(&cmd_buf);
                    }
                }
                KeyCode::Char(c) => {
                    cmd_buf.push(c);
                    cmd_prompt_show(&cmd_buf);
                }
                _ => {}
            }
            continue;
        }

        let mut need_render = true;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,

            KeyCode::Char(':') => {
                cmd_mode = true;
                cmd_buf.clear();
                cmd_prompt_show("");
                need_render = false;
            }

            KeyCode::Char('n') | KeyCode::Char(' ') => viewer.next(),
            KeyCode::Char('p') | KeyCode::Char('b') | KeyCode::Backspace => viewer.prev(),

            KeyCode::Char('+') | KeyCode::Char('=') => viewer.zoom_in(),
            KeyCode::Char('-') => viewer.zoom_out(),
            KeyCode::Char('0') => viewer.reset_view(),

            KeyCode::Left  | KeyCode::Char('h') => viewer.pan_x -= Viewer::PAN_STEP,
            KeyCode::Right | KeyCode::Char('l') => viewer.pan_x += Viewer::PAN_STEP,
            KeyCode::Up    | KeyCode::Char('k') => viewer.pan_y -= Viewer::PAN_STEP,
            KeyCode::Down  | KeyCode::Char('j') => viewer.pan_y += Viewer::PAN_STEP,

            _ => { need_render = false; }
        }

        if need_render {
            viewer.render(fb)?;
        }
    }
    Ok(())
}
