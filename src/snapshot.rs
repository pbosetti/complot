//! Test helper: renders egui frames to PNG without a window, so that the GUI
//! can be looked at headlessly. Frames are tessellated by egui itself and the
//! triangles filled on the CPU, so the image is what a GPU would draw, text
//! included.

use eframe::egui;
use std::collections::HashMap;
use std::io;
use std::path::Path;

/// An RGBA image, 8 bits per channel, rows from top to bottom.
pub struct Image {
  pub width: usize,
  pub height: usize,
  pub rgba: Vec<u8>,
}

impl Image {
  /// Colour of pixel (x, y) as `[r, g, b, a]`.
  pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
    let i = (y * self.width + x) * 4;
    [
      self.rgba[i],
      self.rgba[i + 1],
      self.rgba[i + 2],
      self.rgba[i + 3],
    ]
  }

  /// Writes the image as a PNG file, uncompressed, without dependencies.
  pub fn write_png(&self, path: &Path) -> io::Result<()> {
    std::fs::write(path, self.to_png()?)
  }

  /// The PNG file contents: 8-bit RGBA, stored (uncompressed) deflate blocks.
  fn to_png(&self) -> io::Result<Vec<u8>> {
    if self.width == 0 || self.height == 0 || self.rgba.len() != self.width * self.height * 4 {
      return Err(invalid_input("image size does not match its pixels"));
    }
    let width = u32::try_from(self.width).map_err(|_| invalid_input("image too wide"))?;
    let height = u32::try_from(self.height).map_err(|_| invalid_input("image too tall"))?;
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // Bit depth 8, colour type 6 (RGBA), deflate, filter method 0, no interlace.
    header.extend_from_slice(&[8, 6, 0, 0, 0]);

    // Each scanline starts with filter type 0 (none).
    let row = self.width * 4;
    let mut raw = Vec::with_capacity((row + 1) * self.height);
    for line in self.rgba.chunks(row) {
      raw.push(0);
      raw.extend_from_slice(line);
    }

    let mut png = PNG_SIGNATURE.to_vec();
    push_chunk(&mut png, b"IHDR", &header);
    push_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    push_chunk(&mut png, b"IEND", &[]);
    Ok(png)
  }
}

const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

/// Largest payload of a stored deflate block.
const STORED_MAX: usize = 65535;

/// The textures egui has sent so far, so that any later frame can be rendered.
#[derive(Default)]
pub struct Renderer {
  textures: HashMap<egui::TextureId, egui::ColorImage>,
}

impl Renderer {
  /// Applies the texture changes of a frame. Call it for every frame of the
  /// context, or the font texture of later frames will be incomplete. Clear
  /// `delta` afterwards: epaint panics when a non-empty delta is dropped.
  pub fn update(&mut self, delta: &egui::TexturesDelta) {
    for (id, deltas) in &delta.set {
      for d in deltas {
        let egui::ImageData::Color(image) = &d.image;
        match d.pos {
          None => {
            self.textures.insert(*id, (**image).clone());
          }
          Some([x, y]) => {
            // egui only sends partial updates to textures it has sent before.
            let Some(tex) = self.textures.get_mut(id) else {
              continue;
            };
            let [w, h] = image.size;
            let stride = tex.size[0];
            for row in 0..h {
              let src = &image.pixels[row * w..(row + 1) * w];
              let start = (y + row) * stride + x;
              tex.pixels[start..start + w].copy_from_slice(src);
            }
          }
        }
      }
    }
    for id in &delta.free {
      self.textures.remove(id);
    }
  }

  /// Renders a frame of `ctx` (after `update` with its textures) to an image
  /// the size of the screen in physical pixels.
  pub fn render(&self, ctx: &egui::Context, mut output: egui::FullOutput) -> Image {
    // Its texture changes were applied by `update`. Epaint panics on a
    // non-empty delta being dropped, so mark it handled.
    output.textures_delta.clear();
    let ppp = output.pixels_per_point;
    let screen = ctx.viewport_rect().size() * ppp;
    let size = [screen.x.ceil() as usize, screen.y.ceil() as usize];
    // Opaque, so premultiplied and straight colours agree.
    let [r, g, b, _] = unit(ctx.global_style().visuals.panel_fill);
    let mut buf = vec![[r, g, b, 1.0]; size[0] * size[1]];
    for clipped in ctx.tessellate(output.shapes, ppp) {
      if let egui::epaint::Primitive::Mesh(mesh) = &clipped.primitive {
        self.draw_mesh(&mut buf, size, ppp, clipped.clip_rect, mesh);
      }
    }
    let rgba = buf
      .iter()
      .flat_map(|px| px.map(|c| (c * 255.0).round().clamp(0.0, 255.0) as u8))
      .collect();
    Image {
      width: size[0],
      height: size[1],
      rgba,
    }
  }

  /// Blends the triangles of `mesh` into `buf`, a premultiplied buffer of
  /// `size` pixels, the way the GPU backends do: in gamma space, over.
  fn draw_mesh(
    &self,
    buf: &mut [[f32; 4]],
    [width, height]: [usize; 2],
    ppp: f32,
    clip: egui::Rect,
    mesh: &egui::Mesh,
  ) {
    let tex = self.textures.get(&mesh.texture_id);
    let (x0, x1) = span(clip.min.x * ppp, clip.max.x * ppp, width);
    let (y0, y1) = span(clip.min.y * ppp, clip.max.y * ppp, height);
    for tri in mesh.indices.as_chunks::<3>().0 {
      let v = tri.map(|i| &mesh.vertices[i as usize]);
      let p = v.map(|q| [q.pos.x * ppp, q.pos.y * ppp]);
      let area = edge(p[0], p[1], p[2]);
      if area == 0.0 || !area.is_finite() {
        continue;
      }
      // Weight k is taken against the edge opposite vertex k, which runs from
      // vertex k+1 to k+2 (reversed if the triangle is clockwise).
      let owns = [0, 1, 2].map(|k| {
        let (a, b) = (p[(k + 1) % 3], p[(k + 2) % 3]);
        if area > 0.0 {
          owns_edge(a, b)
        } else {
          owns_edge(b, a)
        }
      });
      let fold =
        |f: fn(f32, f32) -> f32, init: f32, k: usize| p.iter().fold(init, |m, q| f(m, q[k]));
      let bx0 = (fold(f32::min, f32::INFINITY, 0).floor() as usize).max(x0);
      let bx1 = (fold(f32::max, f32::NEG_INFINITY, 0).ceil() as usize)
        .saturating_add(1)
        .min(x1);
      let by0 = (fold(f32::min, f32::INFINITY, 1).floor() as usize).max(y0);
      let by1 = (fold(f32::max, f32::NEG_INFINITY, 1).ceil() as usize)
        .saturating_add(1)
        .min(y1);
      for y in by0..by1 {
        for x in bx0..bx1 {
          let centre = [x as f32 + 0.5, y as f32 + 0.5];
          let w = [
            edge(p[1], p[2], centre) / area,
            edge(p[2], p[0], centre) / area,
            edge(p[0], p[1], centre) / area,
          ];
          let inside = w
            .iter()
            .zip(owns)
            .all(|(&w, own)| w > EDGE_EPS || (own && w >= -EDGE_EPS));
          if !inside {
            continue;
          }
          let mut uv = [0.0; 2];
          let mut col = [0.0; 4];
          for (wk, vert) in w.iter().zip(&v) {
            uv[0] += wk * vert.uv.x;
            uv[1] += wk * vert.uv.y;
            for (ch, src) in col.iter_mut().zip(unit(vert.color)) {
              *ch += wk * src;
            }
          }
          let texel = tex.map_or([1.0; 4], |t| sample(t, uv));
          let mut src = [0.0; 4];
          for ((s, c), t) in src.iter_mut().zip(col).zip(texel) {
            *s = c * t;
          }
          let dst = &mut buf[y * width + x];
          let keep = 1.0 - src[3];
          for (d, s) in dst.iter_mut().zip(src) {
            *d = s + *d * keep;
          }
        }
      }
    }
  }
}

/// Runs `frames` frames of `draw` on a fresh context with a screen of `size`
/// points at `pixels_per_point`, and renders the last one.
pub fn snapshot(
  size: egui::Vec2,
  pixels_per_point: f32,
  frames: usize,
  mut draw: impl FnMut(&mut egui::Ui),
) -> Image {
  let ctx = egui::Context::default();
  let mut renderer = Renderer::default();
  let mut last = None;
  for frame in 0..frames.max(1) {
    // The scale is passed as the native one, as eframe reports it, so that the
    // zoom factor stays 1. (set_pixels_per_point on a fresh context would
    // rescale the first frame's screen rect.)
    let viewport = egui::ViewportInfo {
      native_pixels_per_point: Some(pixels_per_point),
      ..Default::default()
    };
    let input = egui::RawInput {
      viewports: std::iter::once((egui::ViewportId::ROOT, viewport)).collect(),
      screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
      time: Some(frame as f64 / 60.0),
      predicted_dt: 1.0 / 60.0,
      ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| draw(ui));
    renderer.update(&output.textures_delta);
    output.textures_delta.clear();
    last = Some(output);
  }
  renderer.render(&ctx, last.expect("at least one frame ran"))
}

/// Barycentric weights this close to zero count as lying on an edge.
const EDGE_EPS: f32 = 1e-6;

/// Twice the signed area of the triangle (a, b, c). Its sign is the winding.
fn edge(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
  (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// Whether a pixel centre on the counter-clockwise edge a -> b belongs to the
/// triangle. Of two triangles sharing an edge, exactly one claims it (the
/// top-left rule), so there are neither gaps nor double-blended pixels.
fn owns_edge(a: [f32; 2], b: [f32; 2]) -> bool {
  let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
  dy < 0.0 || (dy == 0.0 && dx < 0.0)
}

/// Pixel range `lo..hi` of a clip range in physical pixels, rounded as the GPU
/// backends round their scissor rects, and clamped to `0..limit`.
fn span(lo: f32, hi: f32, limit: usize) -> (usize, usize) {
  let lo = (lo.round() as usize).min(limit);
  let hi = (hi.round() as usize).clamp(lo, limit);
  (lo, hi)
}

/// A colour as floats in 0..1, premultiplied as egui stores it.
fn unit(c: egui::Color32) -> [f32; 4] {
  c.to_array().map(|v| f32::from(v) / 255.0)
}

/// Bilinear sample of a texture at `uv` (0..1). A degenerate texture is white.
fn sample(tex: &egui::ColorImage, uv: [f32; 2]) -> [f32; 4] {
  let [w, h] = tex.size;
  if w == 0 || h == 0 {
    return [1.0; 4];
  }
  let x = uv[0] * w as f32 - 0.5;
  let y = uv[1] * h as f32 - 0.5;
  let (x0, y0) = (x.floor(), y.floor());
  let (fx, fy) = (x - x0, y - y0);
  let texel = |dx: f32, dy: f32| {
    let ix = ((x0 + dx).max(0.0) as usize).min(w - 1);
    let iy = ((y0 + dy).max(0.0) as usize).min(h - 1);
    unit(tex.pixels[iy * w + ix])
  };
  let mut out = [0.0; 4];
  for (dy, wy) in [(0.0, 1.0 - fy), (1.0, fy)] {
    for (dx, wx) in [(0.0, 1.0 - fx), (1.0, fx)] {
      for (o, v) in out.iter_mut().zip(texel(dx, dy)) {
        *o += v * wx * wy;
      }
    }
  }
  out
}

fn invalid_input(what: &str) -> io::Error {
  io::Error::new(io::ErrorKind::InvalidInput, what)
}

/// Appends a PNG chunk: length, type, data, then CRC-32 over type and data.
fn push_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
  png.extend_from_slice(&(data.len() as u32).to_be_bytes());
  png.extend_from_slice(kind);
  png.extend_from_slice(data);
  let crc = !crc32_update(crc32_update(!0, kind), data);
  png.extend_from_slice(&crc.to_be_bytes());
}

/// CRC-32 lookup table, for the polynomial 0xEDB88320.
const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
  let mut table = [0; 256];
  let mut n = 0;
  while n < 256 {
    let mut c = n as u32;
    let mut k = 0;
    while k < 8 {
      c = if c & 1 == 1 {
        0xEDB8_8320 ^ (c >> 1)
      } else {
        c >> 1
      };
      k += 1;
    }
    table[n] = c;
    n += 1;
  }
  table
}

/// Feeds `bytes` into a running CRC-32. Start from `!0`, complement at the end.
fn crc32_update(crc: u32, bytes: &[u8]) -> u32 {
  bytes.iter().fold(crc, |c, &b| {
    CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
  })
}

/// A zlib stream holding `data` in stored deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
  let mut blocks: Vec<&[u8]> = data.chunks(STORED_MAX).collect();
  if blocks.is_empty() {
    // Even empty data needs one final block.
    blocks.push(&[]);
  }
  let last = blocks.len() - 1;
  // Header: deflate with a 32 KiB window, no preset dictionary, fastest level.
  let mut out = vec![0x78, 0x01];
  for (i, &block) in blocks.iter().enumerate() {
    let len = block.len() as u16;
    // Bit 0 is BFINAL, bits 1-2 are BTYPE 00 (stored).
    out.push(u8::from(i == last));
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(block);
  }
  out.extend_from_slice(&adler32(data).to_be_bytes());
  out
}

/// Adler-32 checksum, as zlib uses it.
fn adler32(data: &[u8]) -> u32 {
  const MOD: u32 = 65521;
  // 5552 is the longest run for which the sums cannot overflow a u32.
  let (mut a, mut b) = (1u32, 0u32);
  for chunk in data.chunks(5552) {
    for &x in chunk {
      a += u32::from(x);
      b += a;
    }
    a %= MOD;
    b %= MOD;
  }
  (b << 16) | a
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::process::Command;

  /// Where `demo_snapshot` writes its image.
  const DEMO_PNG: &str = "/private/tmp/claude-501/-Users-p4010-Develop-complot/3f20c2ed-8d19-4f7f-8cfe-e4b90612c07a/scratchpad/snap-demo.png";

  const RED: [u8; 4] = [255, 0, 0, 255];

  /// The background of an empty frame, opaque.
  fn panel_fill() -> [u8; 4] {
    let [r, g, b, _] = egui::Context::default()
      .global_style()
      .visuals
      .panel_fill
      .to_array();
    [r, g, b, 255]
  }

  /// Whether `px` is within `tol` of `want` in every channel.
  fn near(px: [u8; 4], want: [u8; 4], tol: u8) -> bool {
    px.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= tol)
  }

  /// Number of pixels within `tol` of `want`.
  fn count(img: &Image, want: [u8; 4], tol: u8) -> usize {
    img
      .rgba
      .as_chunks::<4>()
      .0
      .iter()
      .filter(|&&p| near(p, want, tol))
      .count()
  }

  /// Big-endian u32 from the first four bytes of `b`.
  fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
  }

  /// Reads back a PNG from `write_png`, checking the CRCs and the zlib stream.
  /// Returns the width, height and RGBA pixels.
  fn decode(png: &[u8]) -> (usize, usize, Vec<u8>) {
    assert_eq!(&png[..8], PNG_SIGNATURE);
    let (mut pos, mut size, mut idat) = (8, (0, 0), Vec::new());
    loop {
      let len = be32(&png[pos..]) as usize;
      let kind = &png[pos + 4..pos + 8];
      let data = &png[pos + 8..pos + 8 + len];
      let crc = be32(&png[pos + 8 + len..]);
      assert_eq!(crc, !crc32_update(crc32_update(!0, kind), data), "bad CRC");
      match kind {
        b"IHDR" => {
          size = (be32(data) as usize, be32(&data[4..]) as usize);
          assert_eq!(&data[8..], [8, 6, 0, 0, 0]);
        }
        b"IDAT" => idat.extend_from_slice(data),
        b"IEND" => break,
        _ => panic!("unexpected chunk {kind:?}"),
      }
      pos += 12 + len;
    }
    assert_eq!(&idat[..2], [0x78, 0x01]);
    let mut raw = Vec::new();
    let mut p = 2;
    loop {
      let header = idat[p];
      let len = u16::from_le_bytes([idat[p + 1], idat[p + 2]]) as usize;
      assert_eq!(
        u16::from_le_bytes([idat[p + 3], idat[p + 4]]),
        !(len as u16)
      );
      assert_eq!((header >> 1) & 3, 0, "not a stored block");
      raw.extend_from_slice(&idat[p + 5..p + 5 + len]);
      p += 5 + len;
      if header & 1 == 1 {
        break;
      }
    }
    assert_eq!(be32(&idat[p..]), adler32(&raw), "bad Adler-32");
    assert_eq!(p + 4, idat.len());
    let (w, h) = size;
    let mut rgba = Vec::new();
    for line in raw.chunks(w * 4 + 1) {
      assert_eq!(line[0], 0, "filter type");
      rgba.extend_from_slice(&line[1..]);
    }
    (w, h, rgba)
  }

  #[test]
  fn fills_rects_at_one_and_two_pixels_per_point() {
    let rect = egui::Rect::from_min_size(egui::pos2(20.0, 10.0), egui::vec2(50.0, 30.0));
    let paint = |ui: &mut egui::Ui| {
      ui.painter().rect_filled(rect, 0.0, egui::Color32::RED);
    };

    let img = snapshot(egui::vec2(200.0, 100.0), 1.0, 1, paint);
    assert_eq!((img.width, img.height), (200, 100));
    assert!(near(img.pixel(30, 20), RED, 2));
    assert_eq!(img.pixel(0, 0), panel_fill());
    assert_eq!(img.pixel(150, 80), panel_fill());
    assert_eq!(count(&img, RED, 2), 50 * 30);

    // Twice the pixels along each side, so four times the area.
    let img = snapshot(egui::vec2(200.0, 100.0), 2.0, 1, paint);
    assert_eq!((img.width, img.height), (400, 200));
    assert!(near(img.pixel(40, 20), RED, 2));
    assert!(near(img.pixel(139, 79), RED, 2));
    assert_eq!(img.pixel(39, 40), panel_fill());
    assert_eq!(img.pixel(140, 40), panel_fill());
    assert_eq!(count(&img, RED, 2), 100 * 60);
  }

  #[test]
  fn draws_label_text_from_the_font_texture() {
    let mut label = None;
    let img = snapshot(egui::vec2(200.0, 100.0), 1.0, 1, |ui| {
      label = Some(ui.label("Hello").rect);
    });
    let rect = label.expect("the frame drew the label");
    let bg = panel_fill();
    let mut differing = 0;
    for y in rect.min.y.floor() as usize..rect.max.y.ceil() as usize {
      for x in rect.min.x.floor() as usize..rect.max.x.ceil() as usize {
        if !near(img.pixel(x, y), bg, 64) {
          differing += 1;
        }
      }
    }
    assert!(
      differing >= 20,
      "only {differing} pixels of the label differ from the background"
    );
  }

  #[test]
  fn blends_premultiplied_white_over_black() {
    let rect = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(40.0, 40.0));
    let img = snapshot(egui::vec2(100.0, 100.0), 1.0, 1, |ui| {
      ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
      ui.painter()
        .rect_filled(rect, 0.0, egui::Color32::from_white_alpha(128));
    });
    // (30, 30) lies on an edge shared by two of the rect's triangles, so this
    // also checks that the pixel is blended once, not twice.
    assert!(
      near(img.pixel(30, 30), [128, 128, 128, 255], 3),
      "{:?}",
      img.pixel(30, 30)
    );
  }

  #[test]
  fn writes_a_png_that_reads_back() {
    let rect = egui::Rect::from_min_size(egui::pos2(20.0, 10.0), egui::vec2(50.0, 30.0));
    let img = snapshot(egui::vec2(200.0, 100.0), 1.0, 1, |ui| {
      ui.painter().rect_filled(rect, 0.0, egui::Color32::RED);
    });
    let path = std::env::temp_dir().join(format!("complot-snapshot-{}.png", std::process::id()));
    img.write_png(&path).expect("PNG written");
    let bytes = std::fs::read(&path).expect("PNG read back");
    assert_eq!(&bytes[..8], PNG_SIGNATURE);
    assert_eq!(&bytes[12..16], b"IHDR");
    assert_eq!(be32(&bytes[16..]), 200);
    assert_eq!(be32(&bytes[20..]), 100);
    let (w, h, rgba) = decode(&bytes);
    assert_eq!((w, h), (200, 100));
    assert!(rgba == img.rgba, "pixels differ after the round trip");

    // macOS sips reads the size back; skipped where there is no sips.
    if let Ok(out) = Command::new("sips")
      .args(["-g", "pixelWidth", "-g", "pixelHeight"])
      .arg(&path)
      .output()
    {
      let text = String::from_utf8_lossy(&out.stdout);
      assert!(out.status.success(), "sips failed: {text}");
      assert!(
        text.lines().any(|l| l.trim() == "pixelWidth: 200"),
        "{text}"
      );
      assert!(
        text.lines().any(|l| l.trim() == "pixelHeight: 100"),
        "{text}"
      );
    }
    std::fs::remove_file(&path).ok();
  }

  #[test]
  fn png_round_trips_single_and_multiple_blocks() {
    // A single stored block, and one with several (over 64 KiB of raw data).
    for (w, h) in [(3, 2), (300, 200)] {
      let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
      let img = Image {
        width: w,
        height: h,
        rgba,
      };
      let png = img.to_png().expect("valid image");
      assert_eq!(decode(&png), (w, h, img.rgba));
    }
  }

  #[test]
  fn rejects_images_that_do_not_match_their_size() {
    let img = Image {
      width: 2,
      height: 2,
      rgba: vec![0; 15],
    };
    assert!(img.to_png().is_err());
  }

  #[test]
  fn checksums_match_reference_values() {
    assert_eq!(!crc32_update(!0, b"IEND"), 0xAE42_6082);
    assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
  }

  #[test]
  fn partial_texture_updates_land_at_their_offset() {
    let id = egui::TextureId::Managed(7);
    let black = egui::Color32::BLACK;
    let mut renderer = Renderer::default();

    let opts = egui::TextureOptions::LINEAR;
    let mut set = egui::TexturesDelta::default();
    let full = egui::ColorImage::filled([4, 3], black);
    set.push(id, egui::epaint::ImageDelta::full(full, opts));
    renderer.update(&set);
    set.clear();

    let patch = egui::ColorImage::new([2, 1], vec![egui::Color32::WHITE, egui::Color32::RED]);
    let mut partial = egui::TexturesDelta::default();
    partial.push(id, egui::epaint::ImageDelta::partial([1, 2], patch, opts));
    renderer.update(&partial);
    partial.clear();

    let tex = &renderer.textures[&id];
    assert_eq!(tex.size, [4, 3]);
    // Row 2, columns 1 and 2 hold the patch; everything else is untouched.
    assert_eq!(tex.pixels[2 * 4 + 1], egui::Color32::WHITE);
    assert_eq!(tex.pixels[2 * 4 + 2], egui::Color32::RED);
    assert_eq!(tex.pixels.iter().filter(|&&p| p != black).count(), 2);

    let mut free = egui::TexturesDelta::default();
    free.free(id);
    renderer.update(&free);
    free.clear();
    assert!(renderer.textures.is_empty());
  }

  /// The UI of `demo_snapshot`: text, widgets and painter shapes.
  fn demo_ui(ui: &mut egui::Ui, checked: &mut bool, gain: &mut f64) {
    ui.heading("complot snapshot");
    ui.label("Text, widgets and painter shapes, rasterised on the CPU.");
    ui.horizontal(|ui| {
      let _ = ui.button("Button");
      ui.checkbox(checked, "Checkbox");
    });
    ui.add(egui::Slider::new(gain, 0.0..=10.0).text("Slider"));

    let (rect, _) = ui.allocate_exact_size(egui::vec2(480.0, 240.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let amp = *gain as f32 * 10.0;
    let wave: Vec<egui::Pos2> = (0..=120)
      .map(|i| {
        let t = i as f32 / 120.0;
        let y = rect.center().y - (t * std::f32::consts::TAU * 2.0).sin() * amp;
        egui::pos2(rect.left() + t * rect.width(), y)
      })
      .collect();
    p.add(egui::Shape::line(
      wave,
      egui::Stroke::new(2.0, egui::Color32::LIGHT_BLUE),
    ));
    p.line_segment(
      [rect.left_bottom(), rect.right_bottom()],
      egui::Stroke::new(1.0, egui::Color32::GRAY),
    );
    p.circle_filled(
      rect.left_top() + egui::vec2(60.0, 60.0),
      34.0,
      egui::Color32::from_rgb(230, 120, 60),
    );
    p.circle_stroke(
      rect.right_top() + egui::vec2(-80.0, 80.0),
      50.0,
      egui::Stroke::new(3.0, egui::Color32::LIGHT_GREEN),
    );
  }

  /// `demo_ui` at 2 pixels per point, written to `DEMO_PNG` for a look.
  /// Run with `cargo test demo_snapshot -- --ignored`.
  #[test]
  #[ignore = "writes a PNG to look at; run with --ignored"]
  fn demo_snapshot() {
    let (mut checked, mut gain) = (true, 4.0);
    let img = snapshot(egui::vec2(900.0, 560.0), 2.0, 2, |ui| {
      demo_ui(ui, &mut checked, &mut gain)
    });
    img
      .write_png(Path::new(DEMO_PNG))
      .expect("demo PNG written");
  }
}
