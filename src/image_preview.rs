//! Inline image previews shown beside tab completions that are image paths.
//!
//! Uses the kitty graphics protocol with Unicode placeholders (`U=1`): the image
//! is transmitted once as a virtual placement, and then drawn by writing
//! `U+10EEEE` placeholder characters (with row/column diacritics, and the image
//! id encoded in the foreground colour) into normal ratatui cells. Because the
//! placeholders are ordinary text, this works inside tmux (with
//! `allow-passthrough on`) and plays nicely with flyline's diff-based rendering.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine;
use ratatui::style::{Color, Style};

use crate::subshell_ipc::{IpcStatus, SubshellHandle, spawn_subshell};
use crate::tag::{Contents, Tag};

const PLACEHOLDER: char = '\u{10EEEE}';
pub const MAX_ROWS: u16 = 14;
pub const MAX_COLS: u16 = 48;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Row/column diacritics from kitty's `rowcolumn-diacritics.txt` (index = row/col number).
const DIACRITICS: [char; 55] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}',
];

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Whether the terminal (possibly behind tmux) is one known to support kitty
/// graphics Unicode placeholders.
pub fn terminal_supported() -> bool {
    if std::env::var_os("FLYLINE_IMAGE_PREVIEW").is_some_and(|v| v == "0") {
        return false;
    }
    if std::env::var_os("FLYLINE_IMAGE_PREVIEW").is_some_and(|v| v == "1") {
        return true;
    }
    if crate::term_info::is_ghostty() || crate::term_info::is_kitty() {
        return true;
    }
    // Behind tmux the terminal query is answered by tmux itself, so fall back
    // to the variables Ghostty/kitty export into the session environment.
    let term = std::env::var("TERM").unwrap_or_default();
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
        || term == "xterm-kitty"
        || term == "xterm-ghostty"
}

/// Cheap check (no filesystem access) whether a word looks like an image path.
pub fn has_image_extension(word: &str) -> bool {
    let word = word.trim_end_matches(['\'', '"']);
    Path::new(word)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Return `path` if it is an existing image file of a reasonable size.
pub fn existing_image(path: &Path) -> Option<PathBuf> {
    if !has_image_extension(path.to_str()?) {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    (meta.is_file() && meta.len() <= MAX_FILE_BYTES).then(|| path.to_path_buf())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Loaded {
    cols: u16,
    rows: u16,
    /// Kitty graphics escape sequence(s) that transmit the image (pure ASCII).
    escape: String,
}

enum State {
    /// Decoding runs in a forked subshell: flyline lives inside the bash
    /// process and must not spawn threads (see clippy.toml).
    Loading(SubshellHandle<Loaded>),
    Ready {
        cols: u16,
        rows: u16,
    },
    Failed,
}

struct Preview {
    path: PathBuf,
    max_box: (u16, u16),
    id: u32,
    state: State,
    transmitted: bool,
}

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// Allocate a terminal image id. Ids are encoded in a 24-bit RGB foreground
/// colour, so they must fit in 24 bits and be non-zero.
fn alloc_id() -> u32 {
    let _ = NEXT_ID.compare_exchange(
        0,
        ((std::process::id() & 0xFFFF) << 8) | 1,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    loop {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) & 0x00FF_FFFF;
        if id != 0 {
            return id;
        }
    }
}

#[derive(Default)]
pub struct ImagePreviewManager {
    current: Option<Preview>,
}

impl ImagePreviewManager {
    /// Set (or clear) the image to preview, fitted into at most
    /// `max_cols` x `max_rows` cells. Returns true if anything changed.
    pub fn set_target(&mut self, path: Option<PathBuf>, max_cols: u16, max_rows: u16) -> bool {
        let max_box = (max_cols.clamp(1, MAX_COLS), max_rows.clamp(1, MAX_ROWS));
        if self.current.as_ref().map(|p| (&p.path, p.max_box))
            == path.as_ref().map(|p| (p, max_box))
        {
            return false;
        }
        self.clear();
        let Some(path) = path else {
            return true;
        };

        let id = alloc_id();
        let (cell_w, cell_h) = cell_pixel_size();
        let load_path = path.clone();
        let multiplexed = in_tmux();
        let Some(handle) = spawn_subshell(move || {
            load_image(&load_path, id, cell_w, cell_h, max_box, multiplexed)
        }) else {
            return true;
        };
        self.current = Some(Preview {
            path,
            max_box,
            id,
            state: State::Loading(handle),
            transmitted: false,
        });
        true
    }

    /// Poll the background decoder. On completion, transmits the image to the
    /// terminal. Returns true if a redraw is needed.
    pub fn poll(&mut self) -> bool {
        let Some(preview) = self.current.as_mut() else {
            return false;
        };
        let State::Loading(handle) = &preview.state else {
            return false;
        };
        match handle.receiver.poll_status() {
            IpcStatus::Empty => return false,
            IpcStatus::Ready(loaded) => {
                let mut out = std::io::stdout();
                if out
                    .write_all(loaded.escape.as_bytes())
                    .and_then(|_| out.flush())
                    .is_ok()
                {
                    preview.transmitted = true;
                    preview.state = State::Ready {
                        cols: loaded.cols,
                        rows: loaded.rows,
                    };
                } else {
                    preview.state = State::Failed;
                }
            }
            IpcStatus::Disconnected => {
                log::debug!("Image preview failed for {}", preview.path.display());
                preview.state = State::Failed;
            }
        }
        true
    }

    /// Size in cells of the loaded image, once it is ready to draw.
    pub fn ready_size(&self) -> Option<(u16, u16)> {
        match self.current.as_ref()?.state {
            State::Ready { cols, rows } => Some((cols, rows)),
            _ => None,
        }
    }

    /// Draw the placeholder grid with its top-left corner at (`row`, `col`),
    /// growing the content buffer if needed.
    pub fn render_at(&self, content: &mut Contents, top: u16, left: u16) {
        let Some(preview) = &self.current else {
            return;
        };
        let Some((cols, rows)) = self.ready_size() else {
            return;
        };
        let cols = cols.min(content.width.saturating_sub(left));
        if cols == 0 {
            return;
        }
        while content.buf.len() < (top + rows) as usize {
            content.increase_buf_single_row();
        }
        let id = preview.id;
        let style = Style::default().fg(Color::Rgb(
            ((id >> 16) & 0xFF) as u8,
            ((id >> 8) & 0xFF) as u8,
            (id & 0xFF) as u8,
        ));
        let mut symbol = String::with_capacity(12);
        for r in 0..rows {
            if let Some(line) = content.buf.get_mut((top + r) as usize) {
                for c in 0..cols {
                    symbol.clear();
                    symbol.push(PLACEHOLDER);
                    symbol.push(DIACRITICS[r as usize]);
                    symbol.push(DIACRITICS[c as usize]);
                    if let Some(cell) = line.get_mut((left + c) as usize) {
                        cell.cell.reset();
                        cell.cell.set_symbol(&symbol).set_style(style);
                        cell.tag = Tag::Tooltip;
                        cell.is_continuation = false;
                    }
                }
            }
        }
    }

    /// Forget the current preview and free it in the terminal.
    pub fn clear(&mut self) {
        if let Some(preview) = self.current.take()
            && preview.transmitted
        {
            let cmd = format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", preview.id);
            let mut out = std::io::stdout();
            let _ = out.write_all(&wrap_for_tmux(cmd.as_bytes(), in_tmux()));
            let _ = out.flush();
        }
    }
}

impl Drop for ImagePreviewManager {
    fn drop(&mut self) {
        self.clear();
    }
}

/// (cell width px, cell height px), falling back to a typical 1:2 cell.
fn cell_pixel_size() -> (f32, f32) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 && ws.ws_xpixel > 0 && ws.ws_ypixel > 0 {
        (
            ws.ws_xpixel as f32 / ws.ws_col as f32,
            ws.ws_ypixel as f32 / ws.ws_row as f32,
        )
    } else {
        (10.0, 20.0)
    }
}

fn load_image(
    path: &Path,
    id: u32,
    cell_w: f32,
    cell_h: f32,
    (max_cols, max_rows): (u16, u16),
    multiplexed: bool,
) -> Option<Loaded> {
    let img = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .map_err(|e| log::debug!("Image decode failed: {}", e))
        .ok()?;
    let (w, h) = (img.width() as f32, img.height() as f32);
    if w == 0.0 || h == 0.0 {
        return None;
    }

    // Fit image into max_cols x max_rows cells, preserving aspect ratio.
    let scale = (max_cols as f32 * cell_w / w)
        .min(max_rows as f32 * cell_h / h)
        .min(1.0);
    let cols = ((w * scale / cell_w).round() as u16).clamp(1, max_cols);
    let rows = ((h * scale / cell_h).round() as u16).clamp(1, max_rows);

    // Downscale before sending so we don't push megabytes through the pty.
    let target_w = (cols as f32 * cell_w).ceil() as u32;
    let target_h = (rows as f32 * cell_h).ceil() as u32;
    let img = if img.width() > target_w || img.height() > target_h {
        img.thumbnail(target_w, target_h)
    } else {
        img
    };

    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);

    let mut escape = Vec::with_capacity(b64.len() + 1024);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        let mut seq = Vec::with_capacity(chunk.len() + 64);
        if i == 0 {
            seq.extend_from_slice(
                format!(
                    "\x1b_Ga=T,U=1,f=100,t=d,i={},c={},r={},q=2,m={};",
                    id, cols, rows, more
                )
                .as_bytes(),
            );
        } else {
            seq.extend_from_slice(format!("\x1b_Gm={};", more).as_bytes());
        }
        seq.extend_from_slice(chunk);
        seq.extend_from_slice(b"\x1b\\");
        escape.extend_from_slice(&wrap_for_tmux(&seq, multiplexed));
    }
    let escape = String::from_utf8(escape).ok()?;
    Some(Loaded { cols, rows, escape })
}

fn in_tmux() -> bool {
    matches!(
        crate::term_info::multiplexer(),
        Some(crate::term_info::Multiplexer::Tmux)
    )
}

fn wrap_for_tmux(seq: &[u8], multiplexed: bool) -> Vec<u8> {
    if !multiplexed {
        return seq.to_vec();
    }
    let mut out = Vec::with_capacity(seq.len() + 16);
    out.extend_from_slice(b"\x1bPtmux;");
    for &b in seq {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn placeholder_is_single_width() {
        let s: String = [PLACEHOLDER, DIACRITICS[3], DIACRITICS[7]].iter().collect();
        assert_eq!(s.width(), 1);
    }

    #[test]
    fn detects_only_existing_images() {
        let dir = std::env::temp_dir().join(format!("flyline-imgtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a b.PNG"), b"x").unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        assert!(existing_image(&dir.join("a b.PNG")).is_some());
        assert!(existing_image(&dir.join("missing.png")).is_none());
        assert!(existing_image(&dir.join("a.txt")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
