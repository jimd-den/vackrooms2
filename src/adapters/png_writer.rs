//! A minimal, dependency-free PNG encoder for debug imagery.
//!
//! The generator's verification surfaces are drawings, and a drawing has to
//! be openable without ceremony. `plan_view` and `section_view` emit SVG
//! because they draw a handful of shapes; a sampled slice is a raster of a
//! million pixels, which SVG cannot carry and ASCII cannot resolve.
//!
//! Rather than take an image dependency into a crate that deliberately has
//! almost none — and that must keep compiling to `wasm32-unknown-unknown` —
//! this writes the PNG by hand. Deflate is used in *stored* mode: no
//! compression, so there is no compressor to be wrong. A debug slice is
//! written once and looked at once; the few megabytes it costs are worth
//! more than a dependency and a lifetime of keeping it current.

/// Encodes an RGB image (`3 * width * height` bytes, row-major) as PNG.
pub fn encode_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(
        rgb.len(),
        (width as usize) * (height as usize) * 3,
        "pixel buffer does not match the stated size"
    );

    // PNG wants a filter byte at the head of every scanline. Zero means
    // "no filter", which is what an uncompressed image wants anyway.
    let mut raw = Vec::with_capacity(rgb.len() + height as usize);
    for row in 0..height as usize {
        raw.push(0u8);
        let start = row * width as usize * 3;
        raw.extend_from_slice(&rgb[start..start + width as usize * 3]);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolour RGB
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut crc = Crc::new();
    crc.write(kind);
    crc.write(body);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// A zlib stream whose deflate blocks are all stored (uncompressed).
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    // CMF/FLG for deflate with a 32K window and no preset dictionary.
    let mut out = vec![0x78, 0x01];
    // A stored block carries at most u16::MAX bytes, so long images need
    // several; only the last may set the final-block flag.
    let mut chunks = data.chunks(0xFFFF).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    while let Some(part) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = part.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(part);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

struct Crc(u32);

impl Crc {
    fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= byte as u32;
            for _ in 0..8 {
                self.0 = if self.0 & 1 != 0 {
                    (self.0 >> 1) ^ 0xEDB8_8320
                } else {
                    self.0 >> 1
                };
            }
        }
    }

    fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes a decoder checks first. A file that fails here is not a
    /// PNG at all, whatever its extension says.
    #[test]
    fn the_file_is_a_png_a_decoder_would_accept() {
        let png = encode_rgb(2, 2, &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..20], &2u32.to_be_bytes());
        assert_eq!(&png[20..24], &2u32.to_be_bytes());
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    /// Every chunk carries a CRC, and a decoder rejects the file if one is
    /// wrong. Walking the chunk list also proves the lengths are honest.
    #[test]
    fn every_chunk_checks_out() {
        let png = encode_rgb(37, 11, &vec![0x5A; 37 * 11 * 3]);
        let mut at = 8;
        let mut kinds = Vec::new();
        while at < png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind = &png[at + 4..at + 8];
            let mut crc = Crc::new();
            crc.write(kind);
            crc.write(&png[at + 8..at + 8 + len]);
            let stored = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
            assert_eq!(crc.finish(), stored, "bad CRC on {:?}", kind);
            kinds.push(String::from_utf8_lossy(kind).to_string());
            at += 12 + len;
        }
        assert_eq!(kinds, ["IHDR", "IDAT", "IEND"]);
        assert_eq!(at, png.len(), "chunk lengths do not span the file");
    }

    /// An image wider than one stored deflate block still produces a single
    /// valid stream — the case a naive encoder silently truncates.
    #[test]
    fn an_image_larger_than_one_deflate_block_survives() {
        let (w, h) = (256u32, 256u32);
        let pixels = vec![0x21u8; (w * h * 3) as usize];
        let png = encode_rgb(w, h, &pixels);
        // Raw stream is 256 rows of 1 + 768 bytes = 196 864 > 65 535.
        let raw_len = (h as usize) * (1 + w as usize * 3);
        assert!(raw_len > 0xFFFF);
        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        // Two bytes of zlib header, four of adler, plus five per block.
        let blocks = raw_len.div_ceil(0xFFFF);
        assert_eq!(idat_len, 2 + 4 + raw_len + blocks * 5);
    }

    #[test]
    fn adler_matches_the_reference_value() {
        // zlib's own documented example.
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
