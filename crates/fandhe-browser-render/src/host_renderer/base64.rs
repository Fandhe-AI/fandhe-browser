//! 厳密な標準 base64 デコーダ（RENDER-3・TASK-38・#235）。
//!
//! `host_renderer` が、ホスト（untrusted）の `screenshot` 応答の `data_base64` を画像バイト列へ
//! 戻すために使う。新しい外部依存を増やさない方針（dependency-policy）で std だけで実装する。
//! 標準アルファベット（`+` `/`）のみ受け付け、空白・改行・URL-safe 文字は拒否する。
//! 長さは 4 の倍数、パディング `=` は末尾の最大 2 文字だけ。出力サイズは確保の前に計算して
//! 上限と比べる（無制限確保による DoS の防止）。

/// デコード後の上限（バイト）。protocol 側の base64 上限 24 MiB（25_165_824）の 3/4。
pub(super) const MAX_DECODED_BYTES: usize = 25_165_824 / 4 * 3;

/// デコード失敗。詳細（入力断片）は持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DecodeError;

fn sextet(b: u8) -> Option<u32> {
    match b {
        b'A'..=b'Z' => Some(u32::from(b - b'A')),
        b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// 厳密にデコードする。空入力・不正文字・不正パディング・上限超過は `Err`。
pub(super) fn decode(input: &str, max_decoded: usize) -> Result<Vec<u8>, DecodeError> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return Err(DecodeError);
    }
    let pad = bytes.iter().rev().take_while(|&&b| b == b'=').count();
    if pad > 2 {
        return Err(DecodeError);
    }
    // 確保の前に出力サイズを算出して上限と比べる。
    let out_len = (bytes.len() / 4 * 3).checked_sub(pad).ok_or(DecodeError)?;
    if out_len > max_decoded {
        return Err(DecodeError);
    }
    // パディング以外の `=` が本体に混ざっていないこと。
    let body = bytes.get(..bytes.len() - pad).ok_or(DecodeError)?;
    if body.contains(&b'=') {
        return Err(DecodeError);
    }
    let mut out = Vec::with_capacity(out_len);
    for chunk in bytes.as_chunks::<4>().0 {
        let mut acc: u32 = 0;
        for &b in chunk {
            acc <<= 6;
            if b != b'=' {
                acc |= sextet(b).ok_or(DecodeError)?;
            }
        }
        let triple = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        let take = out_len.saturating_sub(out.len()).min(3);
        out.extend_from_slice(triple.get(..take).ok_or(DecodeError)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = MAX_DECODED_BYTES;

    /// RENDER-3: PNG シグネチャ 8 バイトを復元する。
    #[test]
    fn render_3_decodes_png_signature() {
        assert_eq!(
            decode("iVBORw0KGgo=", MAX).expect("ok"),
            vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
    }

    /// RENDER-3: パディング 0・1・2 個の境界値。
    #[test]
    fn render_3_decodes_padding_variants() {
        assert_eq!(decode("TWFu", MAX).expect("ok"), b"Man");
        assert_eq!(decode("TWE=", MAX).expect("ok"), b"Ma");
        assert_eq!(decode("TQ==", MAX).expect("ok"), b"M");
    }

    /// RENDER-3: 不正入力はすべて Err。
    #[test]
    fn render_3_rejects_malformed_input() {
        for bad in [
            "", "TWF", "TWFuu", "TW*u", "TW Fu", "TWFu\n", "T=Fu", "=WFu", "TQ=A", "T===", "====",
            "TWE=TWFu", "TW-_", "TQ==TQ==",
        ] {
            assert_eq!(
                decode(bad, MAX),
                Err(DecodeError),
                "{bad:?} は拒否されるはず"
            );
        }
    }

    /// RENDER-3: 出力上限を超える入力は確保前に拒否される。
    #[test]
    fn render_3_rejects_over_limit_before_allocation() {
        assert_eq!(decode("TWFu", 2), Err(DecodeError));
        assert!(decode("TWFu", 3).is_ok());
        assert_eq!(decode("TQ==", 0), Err(DecodeError));
    }
}
