//! スクリーンショットの PNG エンコードと base64 化（Servo 非依存の純関数群）。
//!
//! 役割: `page::ServoHandler::screenshot` が Servo から得た RGBA 画素列を、stdio プロトコルの
//! `screenshot` 結果（`{format, width, height, data_base64}`）へ変換する層（`RENDER-3`・TASK-38.3・
//! #232。設計 §2.2・§2.4）。Servo を import しないので、単体テストだけで上限・エンコードを検証できる。
//!
//! 上限の方針（無制限確保による DoS を防ぐ）: 寸法は最大 [`MAX_DIMENSION`]、PNG は
//! [`MAX_PNG_BYTES`]（エンコード中の書き込みでも検査する）、base64 は確保の前に長さを計算して
//! 応答フレームの上限から逆算した値と比べる。超過は成功を装わず [`ScreenshotError`] で返す
//! （`REPAIR-3`）。v1 は一時ファイルを返さず base64 inline のみ（設計 §2.4）。
//! base64 は本体の厳密デコーダ（`host_renderer/base64.rs`）が受理する形式（標準アルファベット・
//! `=` パディング・改行なし）で出力する。

use std::io::{self, Write};
use std::time::Duration;

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};

use crate::server::{
    ErrorKind, MAX_SCREENSHOT_FRAME_BYTES, SCREENSHOT_TIMEOUT_MS, ScreenshotResult,
};

/// PNG バイト列の上限（16 MiB。設計 §2.4）。
pub(crate) const MAX_PNG_BYTES: usize = 16 * 1024 * 1024;
/// 画像の幅・高さの上限（スキーマ `ScreenshotResult.width/height` の maximum）。
pub(crate) const MAX_DIMENSION: u32 = 16_384;
/// `screenshot` の待機上限（20 秒。`RENDER-4`）。
pub(crate) const SCREENSHOT_TIMEOUT: Duration = Duration::from_millis(SCREENSHOT_TIMEOUT_MS);
/// 応答 JSON の外枠（`{"v":1,"id":..,"result":{..}}`）に見込む余裕バイト数。
const RESPONSE_ENVELOPE_RESERVE: usize = 256;
/// 上限超過を示す `io::Error` のメッセージ（エンコーダ越しに上限超過を見分けるための目印）。
const LIMIT_MESSAGE: &str = "png size limit exceeded";

/// スクリーンショット生成の失敗。いずれも `internal` に写す（スキーマに新しい kind を足さない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum ScreenshotError {
    /// 寸法が 0・上限超過、または画素列の長さと一致しない。
    InvalidDimensions,
    /// PNG または base64 が上限を超えた。
    TooLarge,
    /// PNG エンコーダが失敗した。
    Encode,
}

impl ScreenshotError {
    /// プロトコルの `error.kind` への対応。
    pub(crate) fn kind(self) -> ErrorKind {
        ErrorKind::Internal
    }
}

/// 上限を超える書き込みを拒否する出力先（エンコード中の確保量を抑える）。
struct LimitedWriter {
    buf: Vec<u8>,
    limit: usize,
}

impl Write for LimitedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len().saturating_add(data.len()) > self.limit {
            return Err(io::Error::other(LIMIT_MESSAGE));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// RGBA8 の画素列を PNG にエンコードする。
pub(crate) fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, ScreenshotError> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(ScreenshotError::InvalidDimensions);
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or(ScreenshotError::InvalidDimensions)?;
    if expected != rgba.len() {
        return Err(ScreenshotError::InvalidDimensions);
    }
    let mut sink = LimitedWriter {
        buf: Vec::new(),
        limit: MAX_PNG_BYTES,
    };
    match PngEncoder::new(&mut sink).write_image(rgba, width, height, ExtendedColorType::Rgba8) {
        Ok(()) => Ok(sink.buf),
        Err(image::ImageError::IoError(e)) if e.to_string() == LIMIT_MESSAGE => {
            Err(ScreenshotError::TooLarge)
        }
        Err(_) => Err(ScreenshotError::Encode),
    }
}

/// `n` バイトの base64（パディング付き）の長さ。オーバーフローなら `None`。
pub(crate) fn base64_encoded_len(n: usize) -> Option<usize> {
    n.div_ceil(3).checked_mul(4)
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn symbol(index: u32) -> char {
    ALPHABET
        .get((index & 0x3f) as usize)
        .map_or('A', |b| char::from(*b))
}

/// 標準アルファベット・`=` パディング・改行なしで base64 化する。
///
/// 出力長を確保の前に計算し、`max_len` を超えるなら `TooLarge`。
pub(crate) fn encode_base64(input: &[u8], max_len: usize) -> Result<String, ScreenshotError> {
    let len = base64_encoded_len(input.len()).ok_or(ScreenshotError::TooLarge)?;
    if len > max_len {
        return Err(ScreenshotError::TooLarge);
    }
    let mut out = String::with_capacity(len);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(symbol(n >> 18));
        out.push(symbol(n >> 12));
        out.push(if chunk.len() > 1 { symbol(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { symbol(n) } else { '=' });
    }
    Ok(out)
}

/// RGBA 画素列から `screenshot` の結果（PNG の base64）を作る。
pub(crate) fn build_result(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<ScreenshotResult, ScreenshotError> {
    let png = encode_png(width, height, rgba)?;
    if png.len() > MAX_PNG_BYTES {
        return Err(ScreenshotError::TooLarge);
    }
    let max_b64 = MAX_SCREENSHOT_FRAME_BYTES
        .checked_sub(RESPONSE_ENVELOPE_RESERVE)
        .ok_or(ScreenshotError::TooLarge)?;
    let data_base64 = encode_base64(&png, max_b64)?;
    Ok(ScreenshotResult {
        width,
        height,
        data_base64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x3 の合成画像。画素ごとに異なる既知の値を持つ。
    fn sample() -> Vec<u8> {
        let mut v = Vec::new();
        for y in 0..3u8 {
            for x in 0..4u8 {
                v.extend_from_slice(&[x * 60, y * 100, x * 10 + y, 255 - x]);
            }
        }
        v
    }

    fn decode_b64(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut bits = 0;
        for c in s.bytes().filter(|c| *c != b'=') {
            let v = ALPHABET.iter().position(|a| *a == c).expect("alphabet") as u32;
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
                acc &= (1 << bits) - 1;
            }
        }
        out
    }

    /// RENDER-3: 出力は有効な PNG で、デコードすると入力の画素列と一致する。
    #[test]
    fn encode_png_roundtrips_pixels() {
        let rgba = sample();
        let png = encode_png(4, 3, &rgba).expect("encode");
        assert_eq!(&png[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .expect("decode")
            .to_rgba8();
        assert_eq!((img.width(), img.height()), (4, 3));
        assert_eq!(img.as_raw(), &rgba);
    }

    /// RENDER-3: 結果の寸法と base64 が PNG と対応する。
    #[test]
    fn build_result_matches_png() {
        let rgba = sample();
        let r = build_result(4, 3, &rgba).expect("result");
        assert_eq!((r.width, r.height), (4, 3));
        assert_eq!(decode_b64(&r.data_base64), encode_png(4, 3, &rgba).unwrap());
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode_base64(i.as_bytes(), 100).unwrap(), o);
        }
    }

    #[test]
    fn base64_output_is_strict_for_all_byte_values() {
        let input: Vec<u8> = (0..=255u8).collect();
        let s = encode_base64(&input, 1000).unwrap();
        assert_eq!(s.len() % 4, 0);
        assert!(s.bytes().all(|c| ALPHABET.contains(&c) || c == b'='));
        assert!(!s.trim_end_matches('=').contains('='));
        assert_eq!(decode_b64(&s), input);
    }

    #[test]
    fn base64_len_and_limit_are_checked_before_allocation() {
        assert_eq!(base64_encoded_len(0), Some(0));
        assert_eq!(base64_encoded_len(1), Some(4));
        assert_eq!(base64_encoded_len(2), Some(4));
        assert_eq!(base64_encoded_len(3), Some(4));
        assert_eq!(base64_encoded_len(usize::MAX), None);
        assert_eq!(encode_base64(&[0; 3], 3), Err(ScreenshotError::TooLarge));
        assert!(encode_base64(&[0; 3], 4).is_ok());
    }

    #[test]
    fn invalid_dimensions_are_rejected() {
        let px = [0u8; 4];
        assert_eq!(
            encode_png(0, 1, &px),
            Err(ScreenshotError::InvalidDimensions)
        );
        assert_eq!(
            encode_png(1, 0, &px),
            Err(ScreenshotError::InvalidDimensions)
        );
        assert_eq!(
            encode_png(MAX_DIMENSION + 1, 1, &px),
            Err(ScreenshotError::InvalidDimensions)
        );
        assert_eq!(
            encode_png(2, 1, &px),
            Err(ScreenshotError::InvalidDimensions)
        );
    }

    #[test]
    fn limited_writer_enforces_limit() {
        let mut w = LimitedWriter {
            buf: Vec::new(),
            limit: 4,
        };
        assert_eq!(w.write(&[1, 2, 3, 4]).unwrap(), 4);
        assert!(w.write(&[5]).is_err());
        assert_eq!(w.buf, vec![1, 2, 3, 4]);
    }

    #[test]
    fn errors_map_to_internal_and_timeout_is_20s() {
        for e in [
            ScreenshotError::InvalidDimensions,
            ScreenshotError::TooLarge,
            ScreenshotError::Encode,
        ] {
            assert_eq!(e.kind(), ErrorKind::Internal);
        }
        assert_eq!(SCREENSHOT_TIMEOUT, Duration::from_secs(20));
        assert_eq!(MAX_PNG_BYTES, 16_777_216);
    }
}
