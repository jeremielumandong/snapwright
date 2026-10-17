//! Where a finished capture goes: PNG bytes for the clipboard and a file in the captures
//! folder. Encoding runs on a background thread, never on the UI thread.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};

/// Encodes RGBA pixels as PNG, tuned for speed: screenshots are flat colour, so fast
/// compression with the Up filter is close to the best ratio at a fraction of the time.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(rgba.len() / 4);
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.set_filter(png::Filter::Up);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(out)
}

/// Decodes a PNG to 8-bit RGBA: `(width, height, pixels)`.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "windows", test)),
    allow(dead_code)
)]
pub fn decode_png(png: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("PNG too large")?];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[v, a]| [v, v, v, a])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::Indexed => anyhow::bail!("indexed PNG was not expanded"),
    };
    Ok((info.width, info.height, rgba))
}

/// The folder captures are saved to: `Pictures/Snapwright`, or `~/Snapwright` without one.
pub fn captures_dir() -> Result<PathBuf> {
    let dirs = directories::UserDirs::new().context("no home directory")?;
    let base = dirs
        .picture_dir()
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs.home_dir().to_path_buf());
    Ok(base.join("Snapwright"))
}

/// The folder recordings are saved to: `Movies/Snapwright` (`Videos` on Windows and Linux),
/// or `~/Snapwright` without one.
pub fn recordings_dir() -> Result<PathBuf> {
    let dirs = directories::UserDirs::new().context("no home directory")?;
    let base = dirs
        .video_dir()
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs.home_dir().to_path_buf());
    Ok(base.join("Snapwright"))
}

/// A new file name for a recording in the recordings folder (created if needed).
pub fn recording_path(extension: &str) -> Result<PathBuf> {
    let dir = writable(recordings_dir()?, "Recordings")?;
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    Ok(dir.join(format!("Snapwright {ms}.{extension}")))
}

/// `preferred`, created if needed, when files can be written there; otherwise `fallback` in
/// the app's local data folder. Windows' Controlled Folder Access keeps unknown apps out of
/// Pictures and Videos, and refuses even to create a folder there.
fn writable(preferred: PathBuf, fallback: &str) -> Result<PathBuf> {
    match probe(&preferred) {
        Ok(()) => Ok(preferred),
        Err(e) => {
            let dirs = directories::ProjectDirs::from("", "", "Snapwright")
                .context("no home directory")?;
            let dir = dirs.data_local_dir().join(fallback);
            tracing::warn!(
                folder = %preferred.display(),
                error = %e,
                fallback = %dir.display(),
                "cannot write to the folder; saving to the fallback"
            );
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            Ok(dir)
        }
    }
}

/// Creates `dir` and writes (then removes) an empty file in it.
fn probe(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let test = dir.join(format!(".snapwright-{}", std::process::id()));
    std::fs::write(&test, [])?;
    std::fs::remove_file(&test)
}

/// Where the capture library lives: the platform's data folder for Snapwright.
pub fn library_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "Snapwright").context("no home directory")?;
    Ok(dirs.data_dir().join("library"))
}

/// Saves PNG bytes to a new file in the captures folder and returns its path.
pub fn save(png: &[u8]) -> Result<PathBuf> {
    let dir = writable(captures_dir()?, "Captures")?;
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let path = dir.join(format!("Snapwright {ms}.png"));
    std::fs::write(&path, png).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The system clipboard on Linux, kept for the life of the app: on X11 the owner of the
/// clipboard serves every paste, so dropping it would empty the clipboard.
#[cfg(target_os = "linux")]
pub struct LinuxClipboard(std::sync::Mutex<Option<arboard::Clipboard>>);

#[cfg(target_os = "linux")]
impl LinuxClipboard {
    /// An idle clipboard; connects on first use.
    pub fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    /// Puts an RGBA image on the clipboard. Blocks briefly; call it off the UI thread.
    pub fn set_image(&self, width: u32, height: u32, rgba: Vec<u8>) -> Result<()> {
        let mut slot = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("clipboard poisoned by an earlier panic"))?;
        if slot.is_none() {
            *slot = Some(arboard::Clipboard::new()?);
        }
        let Some(clipboard) = slot.as_mut() else {
            anyhow::bail!("no clipboard");
        };
        clipboard.set_image(arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: rgba.into(),
        })?;
        Ok(())
    }
}

/// A `CF_DIBV5` clipboard bitmap of RGBA pixels: a `BITMAPV5HEADER`, then BGRA rows bottom
/// up (Word and WordPad cannot paste a top-down DIB with a negative height).
#[cfg(any(target_os = "windows", test))]
pub fn dib_v5(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    const HEADER: u32 = 124;
    const BI_BITFIELDS: u32 = 3;
    const LCS_SRGB: u32 = 0x7352_4742;
    const LCS_GM_IMAGES: u32 = 4;
    let row = width as usize * 4;
    let mut out = Vec::with_capacity(HEADER as usize + rgba.len());
    for v in [HEADER, width, height] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    for v in [
        BI_BITFIELDS,
        width * height * 4,
        0, // x pixels per metre
        0, // y pixels per metre
        0, // colours used
        0, // colours important
        0x00ff_0000,
        0x0000_ff00,
        0x0000_00ff,
        0xff00_0000,
        LCS_SRGB,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&[0; 36 + 12]); // endpoints and gamma, unused with sRGB
    for v in [LCS_GM_IMAGES, 0, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for line in rgba.chunks_exact(row).rev() {
        out.extend(
            line.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|&[r, g, b, a]| [b, g, r, a]),
        );
    }
    out
}

/// Puts an image on the Windows clipboard as both a `CF_DIBV5` bitmap and the registered
/// "PNG" format. GPUI writes the PNG format only, which Paint, Office and Win+V history
/// cannot paste; they read bitmaps. Call it off the UI thread.
#[cfg(target_os = "windows")]
pub fn set_windows_image(width: u32, height: u32, rgba: &[u8], png: &[u8]) -> Result<()> {
    use clipboard_win::{Clipboard, formats, raw};
    let dib = dib_v5(width, height, rgba);
    let _open =
        Clipboard::new_attempts(10).map_err(|e| anyhow::anyhow!("opening the clipboard: {e}"))?;
    raw::empty().map_err(|e| anyhow::anyhow!("emptying the clipboard: {e}"))?;
    raw::set_without_clear(formats::CF_DIBV5, &dib)
        .map_err(|e| anyhow::anyhow!("writing the bitmap: {e}"))?;
    if let Some(format) = raw::register_format("PNG") {
        raw::set_without_clear(format.get(), png)
            .map_err(|e| anyhow::anyhow!("writing the PNG: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn png_round_trips() {
        let rgba: Vec<u8> = (0..6 * 4).map(|i| i as u8 * 10).collect();
        let png = encode_png(3, 2, &rgba).unwrap();
        assert_eq!(decode_png(&png).unwrap(), (3, 2, rgba));
    }

    #[test]
    fn dib_is_bottom_up_bgra_after_a_v5_header() {
        // 1x2: a red pixel above a blue one.
        let rgba = [255, 0, 0, 255, 0, 0, 255, 255];
        let dib = dib_v5(1, 2, &rgba);
        let u32_at = |i: usize| u32::from_le_bytes(dib[i..i + 4].try_into().unwrap());
        assert_eq!(dib.len(), 124 + 8);
        assert_eq!((u32_at(0), u32_at(4), u32_at(8)), (124, 1, 2));
        assert_eq!(u32_at(16), 3); // BI_BITFIELDS
        assert_eq!(u32_at(20), 8);
        assert_eq!(u32_at(40), 0x00ff_0000); // red mask
        assert_eq!(u32_at(56), 0x7352_4742); // sRGB
        assert_eq!(u32_at(108), 4); // LCS_GM_IMAGES
        assert_eq!(&dib[124..], &[255, 0, 0, 255, 0, 0, 255, 255][..]);
    }
}
