//! Software-blit fallback for X11 displays where creating or probing a
//! presentable wgpu surface segfaults or produces no visible content.
//!
//! Observed on this host: TigerVNC/Xvnc (no GLX, no DRI3) with the NVIDIA
//! Vulkan ICD. Probing a real XCB window surface crashes inside the
//! driver's `is_surface_supported()`; forcing LavaPipe software Vulkan
//! instead initializes but then fails to present anything (the window
//! stays blank). This fallback renders into a plain off-screen wgpu
//! texture (see `gpui_wgpu::WgpuRenderer::new_offscreen_x11`), reads it
//! back to CPU memory, and pushes the pixels to the real X11 window via
//! `XPutImage`, bypassing native GPU surface presentation entirely.
//!
//! This is the same technique already shipped for `warp-oss` on this host
//! (see `~/bin/warp`'s `WARP_FORCE_OFFSCREEN_X11` handling and the
//! `vnc-software-surface-fallback-on-master` branch of the warp repo) —
//! ported here rather than reusing that code directly since it lives in a
//! separate crate/binary with its own wgpu version and X11 connection.
//!
//! Unlike warp's blitter, this one reuses the window's existing XCB
//! connection (`Rc<XCBConnection>`) instead of opening a second one, since
//! gpui_linux already owns one per window.
//!
//! Performance is intentionally not optimized; this exists so a usable Zed
//! window appears on remote/VNC X servers that lack GLX. Expect low
//! single-digit-to-low-teens fps on a large window.

use std::rc::Rc;

use anyhow::{Context as _, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _, CreateGCAux, Gcontext, ImageFormat};
use x11rb::xcb_ffi::XCBConnection;

/// Pushes raw BGRA pixel buffers from CPU memory to a single X window via
/// `PutImage`, using the connection and depth the window was already
/// created with.
pub(crate) struct X11Blitter {
    conn: Rc<XCBConnection>,
    window: xproto::Window,
    gc: Gcontext,
    /// Depth of the X window's visual (24 or 32). PutImage must match this.
    depth: u8,
    width: u32,
    height: u32,
    /// Maximum payload size of a single PutImage request (bytes). PutImage
    /// has a small fixed-size header; the rest is row data. Chunked by
    /// whole rows so dst_y arithmetic stays integer.
    max_request_bytes: usize,
}

impl X11Blitter {
    pub(crate) fn new(
        conn: &Rc<XCBConnection>,
        window: xproto::Window,
        depth: u8,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        // Maximum request length is in 4-byte units. With BIG-REQUESTS (which
        // x11rb negotiates lazily) this is u32-sized. Cap at 4 MiB per request
        // to keep individual round trips cheap and stay well under any
        // server-side limit.
        conn.prefetch_maximum_request_bytes();
        let server_max = conn.maximum_request_bytes().max(64 * 1024);
        let max_request_bytes = server_max.min(4 * 1024 * 1024);

        let gc = conn.generate_id().context("X11 generate_id (gc) failed")?;
        conn.create_gc(gc, window, &CreateGCAux::default())?
            .check()
            .context("XCreateGC failed")?;

        log::info!(
            "X11 offscreen blit fallback active: window=0x{window:x} depth={depth} \
             max_put_image_chunk={max_request_bytes}"
        );

        Ok(Self {
            conn: Rc::clone(conn),
            window,
            gc,
            depth,
            width: width.max(1),
            height: height.max(1),
            max_request_bytes,
        })
    }

    /// Updates the cached window size. Pixel buffers passed to `blit_bgra`
    /// must match this size.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
    }

    /// Pushes a tightly packed BGRA / BGRX byte buffer to the X window via
    /// one or more `PutImage` requests (chunked by whole rows so each
    /// request stays under `max_request_bytes`).
    ///
    /// `pixels.len()` must equal `width * height * 4`.
    pub(crate) fn blit_bgra(&self, pixels: &[u8]) -> Result<()> {
        let width = self.width;
        let height = self.height;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let bytes_per_row = (width as usize) * 4;
        let expected = bytes_per_row * height as usize;
        if pixels.len() != expected {
            anyhow::bail!(
                "blit_bgra: pixel buffer is {} bytes; expected {expected} ({width}x{height} BGRA)",
                pixels.len()
            );
        }

        // PutImage has ~24 bytes of fixed header; leave generous room for it.
        let payload_budget = self.max_request_bytes.saturating_sub(64);
        let rows_per_chunk = (payload_budget / bytes_per_row).max(1);

        let mut row = 0u32;
        while row < height {
            let chunk_rows = rows_per_chunk.min((height - row) as usize) as u32;
            let start = row as usize * bytes_per_row;
            let end = start + chunk_rows as usize * bytes_per_row;
            let slice = &pixels[start..end];

            self.conn
                .put_image(
                    ImageFormat::Z_PIXMAP,
                    self.window,
                    self.gc,
                    width as u16,
                    chunk_rows as u16,
                    0,          // dst_x
                    row as i16, // dst_y
                    0,          // left_pad
                    self.depth,
                    slice,
                )
                .context("XPutImage send failed")?;
            row += chunk_rows;
        }

        // PutImage is a void request, so there's no reply to await, but flush
        // so the bytes actually reach the X server before the caller returns.
        self.conn.flush().context("X11 flush failed")?;
        Ok(())
    }
}

impl Drop for X11Blitter {
    fn drop(&mut self) {
        // Best-effort GC cleanup; ignore errors during shutdown.
        let _ = self.conn.free_gc(self.gc);
        let _ = self.conn.flush();
    }
}
