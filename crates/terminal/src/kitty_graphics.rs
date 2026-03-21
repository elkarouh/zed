//! Kitty Graphics Protocol (KGP) support for Zed's terminal.
//!
//! Handles parsing APC escape sequences (`ESC _ G ... ESC \`), decoding
//! transmitted image data, and storing images for rendering. Works with
//! yazi's Unicode placeholder mode (`U=1`), where image tiles are placed
//! via `U+10EEEE` characters with diacritics encoding row/column and
//! foreground color encoding the image ID.

use std::collections::HashMap;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use gpui::RenderImage;
use image::{DynamicImage, Frame, RgbaImage};

/// The special Unicode codepoint used by Kitty graphics protocol for
/// virtual/Unicode placements.
pub const KITTY_PLACEHOLDER_CHAR: char = '\u{10EEEE}';

/// Extract the last CUP (Cursor Position) sequence from a byte buffer.
/// CUP format: ESC [ Pn ; Pn H  (1-indexed row;col)
/// Returns (row, col) in 0-indexed terminal coordinates, or None.
pub fn extract_last_cup(bytes: &[u8]) -> Option<(u16, u16)> {
    // Scan backwards for ESC [ ... H
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if bytes[i] == b'H' {
            // Find the matching ESC [
            let mut j = i;
            while j > 0 {
                j -= 1;
                if j + 1 < bytes.len() && bytes[j] == 0x1b && bytes[j + 1] == b'[' {
                    // Parse row;col from bytes[j+2..i]
                    let params = &bytes[j + 2..i];
                    let s = std::str::from_utf8(params).ok()?;
                    let parts: Vec<&str> = s.split(';').collect();
                    let row = parts.first().and_then(|r| r.parse::<u16>().ok()).unwrap_or(1);
                    let col = parts.get(1).and_then(|c| c.parse::<u16>().ok()).unwrap_or(1);
                    // Convert from 1-indexed to 0-indexed
                    return Some((row.saturating_sub(1), col.saturating_sub(1)));
                }
                // Don't search too far back
                if i - j > 20 { break; }
            }
        }
    }
    None
}

/// Extracts KGP APC sequences from a byte buffer, handling sequences that
/// span across multiple buffer reads.
///
/// `pending` is a buffer for incomplete APC data from the previous call.
/// On return, `pending` may contain a partial APC sequence awaiting more data.
///
/// Returns `(filtered_bytes, extracted_apc_payloads)`.
/// If no APC sequences were found or buffered, returns the original bytes unchanged.
pub fn extract_kitty_apc_buffered(
    bytes: &[u8],
    pending: &mut Vec<u8>,
) -> (Vec<u8>, Vec<Vec<u8>>) {
    // Combine any pending data with new bytes
    let data: std::borrow::Cow<[u8]> = if pending.is_empty() {
        std::borrow::Cow::Borrowed(bytes)
    } else {
        pending.extend_from_slice(bytes);
        let combined = std::mem::take(pending);
        std::borrow::Cow::Owned(combined)
    };

    // Quick check: does the buffer contain ESC _ at all?
    if !data.windows(2).any(|w| w == b"\x1b_") {
        return (data.into_owned(), Vec::new());
    }

    let mut filtered = Vec::with_capacity(data.len());
    let mut apcs = Vec::new();
    let mut i = 0;

    while i < data.len() {
        // Look for ESC _ (APC start)
        if i + 1 < data.len() && data[i] == 0x1b && data[i + 1] == b'_' {
            // Check if next byte is 'G' (Kitty graphics)
            if i + 2 < data.len() && data[i + 2] == b'G' {
                // Find the string terminator: ESC \ (0x1b 0x5c) or ST (0x9c)
                let start = i + 3; // skip ESC _ G
                let mut end = start;
                let mut found_st = false;
                while end < data.len() {
                    if data[end] == 0x9c {
                        found_st = true;
                        break;
                    }
                    if end + 1 < data.len() && data[end] == 0x1b && data[end + 1] == 0x5c {
                        found_st = true;
                        break;
                    }
                    end += 1;
                }
                if found_st {
                    apcs.push(data[start..end].to_vec());
                    // Skip past the string terminator
                    if data[end] == 0x9c {
                        i = end + 1;
                    } else {
                        i = end + 2; // ESC \
                    }
                    continue;
                } else {
                    // No terminator found — incomplete sequence, buffer it for next call
                    *pending = data[i..].to_vec();
                    break;
                }
            }
        }
        filtered.push(data[i]);
        i += 1;
    }

    (filtered, apcs)
}

/// Parsed key=value parameters from a KGP command.
#[derive(Debug, Default)]
struct KittyParams {
    action: u8,           // a: t=transmit, T=transmit+display, p=place, d=delete, q=query
    format: u32,          // f: 24=RGB, 32=RGBA, 100=PNG
    width: u32,           // s: pixel width
    height: u32,          // v: pixel height
    image_id: u32,        // i: image number
    more_chunks: bool,    // m: 0=last, 1=more
    quiet: u8,            // q: 0=verbose, 1=quiet on ok, 2=quiet always
    #[allow(dead_code)]
    transmission: u8,     // t: d=direct, f=file, t=temp
    #[allow(dead_code)]
    cursor_move: bool,    // C: 0=move (default), 1=don't move
    #[allow(dead_code)]
    unicode_place: bool,  // U: 0=off, 1=on (Unicode placement)
    delete_what: u8,      // d: a/A=all, i/I=by id, etc.
    z_index: i32,         // z: z-layer index
}

impl KittyParams {
    fn parse(params_str: &[u8]) -> Self {
        let mut p = KittyParams::default();
        p.action = b'T'; // default action

        for pair in params_str.split(|&b| b == b',') {
            if pair.len() < 3 || pair[1] != b'=' {
                continue;
            }
            let key = pair[0];
            let val = &pair[2..];
            match key {
                b'a' => p.action = val[0],
                b'f' => p.format = parse_u32(val),
                b's' => p.width = parse_u32(val),
                b'v' => p.height = parse_u32(val),
                b'i' => p.image_id = parse_u32(val),
                b'm' => p.more_chunks = val[0] == b'1',
                b'q' => p.quiet = val[0] - b'0',
                b't' => p.transmission = val[0],
                b'C' => p.cursor_move = val[0] == b'0',
                b'U' => p.unicode_place = val[0] == b'1',
                b'd' => p.delete_what = val[0],
                b'z' => p.z_index = std::str::from_utf8(val).unwrap_or("0").parse().unwrap_or(0),
                _ => {}
            }
        }
        p
    }
}

fn parse_u32(val: &[u8]) -> u32 {
    let s = std::str::from_utf8(val).unwrap_or("0");
    s.parse().unwrap_or(0)
}

/// A decoded image stored in the graphics system.
pub struct StoredImage {
    pub render_image: Arc<RenderImage>,
    pub width: u32,
    pub height: u32,
}

/// Placement of an image for rendering.
/// For KgpOld: placed at the cursor position at time of display.
/// For Kgp: placed via Unicode placeholders in the cell grid.
#[derive(Clone)]
pub struct ImagePlacement {
    pub image_id: u32,
    pub render_image: Arc<RenderImage>,
    pub width: u32,
    pub height: u32,
    pub use_unicode_placeholders: bool,
    pub z_index: i32,
    /// If true, placement position needs to be resolved from terminal cursor
    pub needs_cursor_position: bool,
    /// Position in cell coordinates
    pub col: u16,
    pub row: u16,
    /// Size in cells (computed from image size and cell size)
    pub cols: u16,
    pub rows: u16,
    /// If true, this is a "clear all" signal, not an actual image
    pub is_clear: bool,
}

/// Manages received Kitty graphics images and pending chunked transfers.
pub struct ImageStorage {
    images: HashMap<u32, StoredImage>,
    /// Accumulator for chunked transfers: image_id -> (params_from_first_chunk, accumulated_base64)
    pending: HashMap<u32, (KittyParams, Vec<u8>)>,
    /// Track the last transfer ID for continuation chunks that omit `i`
    last_transfer_id: u32,
    /// Queue for new placements (drained by make_content into current_display)
    pub placements: Vec<ImagePlacement>,
    /// Currently displayed images — persists across frames until replaced
    pub current_display: Vec<ImagePlacement>,
    pub had_delete: bool,
}

impl ImageStorage {
    pub fn new() -> Self {
        Self {
            images: HashMap::new(),
            pending: HashMap::new(),
            last_transfer_id: 0,
            placements: Vec::new(),
            current_display: Vec::new(),
            had_delete: false,
        }
    }

    /// Process a KGP APC payload (everything between `ESC _ G` and `ESC \`).
    /// Returns an optional response to write back to the PTY.
    pub fn process_command(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        // Split at ';' into params and base64 data
        let (params_bytes, data_bytes) = match payload.iter().position(|&b| b == b';') {
            Some(pos) => (&payload[..pos], &payload[pos + 1..]),
            None => (payload, &[] as &[u8]),
        };

        let params = KittyParams::parse(params_bytes);

        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/kgp_debug.log") {
                writeln!(f, "[KGP-CMD] action='{}', id={}, more={}, quiet={}, fmt={}, params_str={:?}",
                    params.action as char, params.image_id, params.more_chunks, params.quiet,
                    params.format, String::from_utf8_lossy(params_bytes)).ok();
            }
        }

        match params.action {
            b'q' => self.handle_query(&params),
            b't' | b'T' => self.handle_transmit(params, data_bytes),
            b'd' => {
                self.handle_delete(&params);
                None
            }
            _ => None,
        }
    }

    fn handle_query(&self, params: &KittyParams) -> Option<Vec<u8>> {
        // Respond with OK to tell the app KGP is supported
        let id = params.image_id;
        Some(format!("\x1b_Gi={id};OK\x1b\\").into_bytes())
    }

    fn handle_transmit(&mut self, params: KittyParams, data: &[u8]) -> Option<Vec<u8>> {
        // If id is 0 and we have a pending transfer, use the last transfer id
        let id = if params.image_id == 0 && !self.pending.is_empty() {
            self.last_transfer_id
        } else {
            if params.image_id != 0 {
                self.last_transfer_id = params.image_id;
            }
            params.image_id
        };

        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/kgp_debug.log") {
                writeln!(f, "[KGP-TX] id={}, action={}, more={}, data_len={}, pending_has={}, fmt={}",
                    id, params.action as char, params.more_chunks, data.len(),
                    self.pending.contains_key(&id), params.format).ok();
            }
        }

        if params.more_chunks {
            // Accumulate chunked data
            let entry = self.pending.entry(id).or_insert_with(|| (params, Vec::new()));
            entry.1.extend_from_slice(data);
            return None;
        }

        // Final chunk (or single-chunk transfer)
        let (first_params, full_data) = if let Some((first_params, mut accumulated)) =
            self.pending.remove(&id)
        {
            accumulated.extend_from_slice(data);
            (first_params, accumulated)
        } else {
            (params, data.to_vec())
        };

        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/kgp_debug.log") {
                writeln!(f, "[KGP-TX-DECODE] id={}, full_data_len={}, fmt={}, w={}, h={}",
                    id, full_data.len(), first_params.format, first_params.width, first_params.height).ok();
            }
        }

        // Decode base64
        let raw = match BASE64.decode(&full_data) {
            Ok(r) => r,
            Err(e) => {
                {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/kgp_debug.log") {
                        writeln!(f, "[KGP-ERROR] base64 decode error for image {}: {}, data_len={}", id, e, full_data.len()).ok();
                    }
                }
                return self.make_response(first_params.quiet, id, "EBASD64");
            }
        };

        // Decode image based on format
        let rgba = match first_params.format {
            100 => {
                // PNG
                match image::load_from_memory_with_format(&raw, image::ImageFormat::Png) {
                    Ok(img) => img.into_rgba8(),
                    Err(e) => {
                        log::warn!("KGP: PNG decode error for image {id}: {e}");
                        return self.make_response(first_params.quiet, id, "EPNG");
                    }
                }
            }
            32 => {
                // Raw RGBA
                let w = first_params.width;
                let h = first_params.height;
                if w == 0 || h == 0 {
                    return self.make_response(first_params.quiet, id, "EDIM");
                }
                match RgbaImage::from_raw(w, h, raw) {
                    Some(img) => img,
                    None => {
                        return self.make_response(first_params.quiet, id, "EDATA");
                    }
                }
            }
            24 => {
                // Raw RGB — convert to RGBA
                let w = first_params.width;
                let h = first_params.height;
                {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/kgp_debug.log") {
                        writeln!(f, "[KGP-DECODE] f=24 (RGB), w={}, h={}, raw_len={}, expected={}", w, h, raw.len(), w as usize * h as usize * 3).ok();
                    }
                }
                if w == 0 || h == 0 {
                    return self.make_response(first_params.quiet, id, "EDIM");
                }
                let mut rgba_data = Vec::with_capacity((w * h * 4) as usize);
                for pixel in raw.chunks_exact(3) {
                    rgba_data.extend_from_slice(pixel);
                    rgba_data.push(255);
                }
                match RgbaImage::from_raw(w, h, rgba_data) {
                    Some(img) => img,
                    None => {
                        return self.make_response(first_params.quiet, id, "EDATA");
                    }
                }
            }
            _ => {
                return self.make_response(first_params.quiet, id, "EFMT");
            }
        };

        let w = rgba.width();
        let h = rgba.height();

        let frame = Frame::new(DynamicImage::ImageRgba8(rgba).into_rgba8().into());
        let render_image = Arc::new(RenderImage::new(vec![frame]));

        let use_unicode = first_params.unicode_place;

        self.images.insert(id, StoredImage {
            render_image: render_image.clone(),
            width: w,
            height: h,
        });

        // Create a placement for non-Unicode mode (KgpOld)
        if !use_unicode {
            {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/kgp_debug.log") {
                    writeln!(f, "[KGP-STORE-PTR] self={:p}, placements_len_before={}", self as *const _, self.placements.len()).ok();
                }
            }
            self.placements.push(ImagePlacement {
                image_id: id,
                render_image,
                width: w,
                height: h,
                use_unicode_placeholders: false,
                z_index: first_params.z_index,
                needs_cursor_position: true,
                col: 0,
                row: 0,
                cols: 0,
                rows: 0,
                is_clear: false,
            });
        }

        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/kgp_debug.log") {
                writeln!(f, "[KGP-STORE] image id={} size={}x{}, unicode={}, placements={}", id, w, h, use_unicode, self.placements.len()).ok();
            }
        }

        self.make_response(first_params.quiet, id, "OK")
    }

    fn handle_delete(&mut self, params: &KittyParams) {
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/kgp_debug.log") {
                writeln!(f, "[KGP-DELETE] what={}, images_before={}", params.delete_what as char, self.images.len()).ok();
            }
        }
        match params.delete_what {
            b'a' | b'A' => {
                self.images.clear();
                self.pending.clear();
                self.had_delete = true;
                // Don't clear placements here — they need to survive until
                // make_content() drains them for rendering. Yazi sends
                // delete-all immediately after transmit+display, but the
                // rendering hasn't picked up the placement yet.
            }
            b'i' | b'I' => {
                self.images.remove(&params.image_id);
                self.pending.remove(&params.image_id);
            }
            _ => {
                self.images.clear();
                self.pending.clear();
            }
        }
    }

    fn make_response(&self, quiet: u8, id: u32, msg: &str) -> Option<Vec<u8>> {
        if quiet >= 2 {
            return None;
        }
        if quiet >= 1 && msg == "OK" {
            return None;
        }
        Some(format!("\x1b_Gi={id};{msg}\x1b\\").into_bytes())
    }

    /// Look up a stored image by ID.
    pub fn get_image(&self, id: u32) -> Option<&StoredImage> {
        self.images.get(&id)
    }

    /// Check if any images are stored.
    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }
}

/// Extract the image ID from a cell's foreground color (RGB).
/// Yazi encodes: r = (id >> 16) & 0xff, g = (id >> 8) & 0xff, b = id & 0xff
pub fn image_id_from_fg(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Extract row/column index from a diacritic combining character.
/// The diacritics table in yazi starts at U+0305.
pub fn diacritic_to_index(c: char) -> u16 {
    let v = c as u32;
    if v >= 0x0305 {
        (v - 0x0305) as u16
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_kitty_apc_none() {
        let mut pending = Vec::new();
        let (filtered, apcs) = extract_kitty_apc_buffered(b"hello world", &mut pending);
        assert!(apcs.is_empty());
        assert_eq!(filtered, b"hello world");
    }

    #[test]
    fn test_extract_kitty_apc_simple() {
        let mut pending = Vec::new();
        let input = b"\x1b_Ga=q,i=31;AAAA\x1b\\rest";
        let (filtered, apcs) = extract_kitty_apc_buffered(input, &mut pending);
        assert_eq!(filtered, b"rest");
        assert_eq!(apcs.len(), 1);
        assert_eq!(&apcs[0], b"a=q,i=31;AAAA");
    }

    #[test]
    fn test_extract_kitty_apc_mixed() {
        let mut pending = Vec::new();
        let input = b"before\x1b_Ga=d,d=A\x1b\\after";
        let (filtered, apcs) = extract_kitty_apc_buffered(input, &mut pending);
        assert_eq!(filtered, b"beforeafter");
        assert_eq!(apcs.len(), 1);
        assert_eq!(&apcs[0], b"a=d,d=A");
    }

    #[test]
    fn test_parse_params() {
        let p = KittyParams::parse(b"a=T,f=100,s=640,v=480,i=12345,m=1");
        assert_eq!(p.action, b'T');
        assert_eq!(p.format, 100);
        assert_eq!(p.width, 640);
        assert_eq!(p.height, 480);
        assert_eq!(p.image_id, 12345);
        assert!(p.more_chunks);
    }

    #[test]
    fn test_image_id_from_fg() {
        assert_eq!(image_id_from_fg(0x01, 0x02, 0x03), 0x010203);
        assert_eq!(image_id_from_fg(0xff, 0xff, 0xff), 0xffffff);
    }

    #[test]
    fn test_query_response() {
        let mut storage = ImageStorage::new();
        let resp = storage.process_command(b"a=q,i=31,s=1,v=1,t=d,f=24;AAAA");
        assert_eq!(resp, Some(b"\x1b_Gi=31;OK\x1b\\".to_vec()));
    }
}
