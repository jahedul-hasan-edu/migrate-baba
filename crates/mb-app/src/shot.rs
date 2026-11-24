//! Debug builds only: `FERRY_SHOT=out.png` renders a few frames, saves a screenshot and exits.
//! `FERRY_SCREEN=connections|doctor|compare|migration` picks the screen. Used for UI review.

use eframe::egui;

pub struct Shot {
    path: String,
    frames: u32,
    requested: bool,
}

impl Shot {
    pub fn from_env() -> Option<Self> {
        std::env::var("FERRY_SHOT").ok().map(|path| Self { path, frames: 0, requested: false })
    }

    pub fn tick(&mut self, ctx: &egui::Context) {
        self.frames += 1;
        ctx.request_repaint();
        let delay: u32 = std::env::var("FERRY_SHOT_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
        if self.frames >= delay && !self.requested {
            self.requested = true;
            let path = self.path.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let mut rgba = Vec::with_capacity(img.pixels.len() * 4);
                for p in &img.pixels {
                    rgba.extend_from_slice(&p.to_srgba_unmultiplied());
                }
                let _ = std::fs::write(&path, png(img.size[0] as u32, img.size[1] as u32, &rgba));
                std::process::exit(0);
            })));
        }
        if self.frames > 6000 {
            std::process::exit(3);
        }
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// Minimal PNG encoder (RGBA8, stored deflate blocks).
fn png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * w as usize * 4..(y + 1) * w as usize * 4]);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65535).peekable();
    while let Some(c) = chunks.next() {
        z.push(if chunks.peek().is_none() { 1 } else { 0 });
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}
