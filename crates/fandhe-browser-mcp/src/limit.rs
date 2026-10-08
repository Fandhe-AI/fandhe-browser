//! stdin の 1 メッセージ（改行区切り 1 行）あたりの長さ上限（TASK-94.2・PLUG-3・MS-9）。
//!
//! `main.rs` が rmcp の stdio トランスポートへ渡す stdin をこの `LimitedReader` で包む。
//! 改行なしの巨大入力で rmcp 側の行バッファが際限なく伸びるのを防ぐため、上限を超えた
//! 時点で `io::Error` を返して読み取りを打ち切り、トランスポートを閉じてセッションを終了させる。
//! 外部入力の無制限確保による DoS 対策（security.md「不安全な設計」）。

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

/// 1 メッセージ（改行を除く 1 行）の最大バイト数。4 MiB。
pub(crate) const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// 直近の改行以降の累積長 `current` に `chunk` を加え、各行が `limit` 以内かを検査する。
/// 上限超過なら `None`、そうでなければ次回へ持ち越す改行以降の長さを返す。
fn advance(current: usize, chunk: &[u8], limit: usize) -> Option<usize> {
    let mut cur = current;
    for (i, seg) in chunk.split(|b| *b == b'\n').enumerate() {
        if i > 0 {
            cur = 0;
        }
        cur = cur.checked_add(seg.len())?;
        if cur > limit {
            return None;
        }
    }
    Some(cur)
}

/// 行ごとの長さを監視する `AsyncRead` ラッパー。上限超過で `InvalidData` を返す。
pub(crate) struct LimitedReader<R> {
    inner: R,
    limit: usize,
    current: usize,
}

impl<R> LimitedReader<R> {
    pub(crate) fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            limit,
            current: 0,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for LimitedReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let (current, limit) = (self.current, self.limit);
                let new = buf.filled().get(before..).unwrap_or_default();
                match advance(current, new, limit) {
                    Some(next) => {
                        self.current = next;
                        Poll::Ready(Ok(()))
                    }
                    None => {
                        // エラー時は読み込み済みバイトを呼び出し側へ渡さない。
                        buf.set_filled(before);
                        Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "message exceeds size limit",
                        )))
                    }
                }
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// PLUG-3 / TASK-94.2: 上限ちょうどの行は許容し、1 バイト超過で拒否する。
    #[test]
    fn plug3_advance_enforces_limit_per_line() {
        assert_eq!(advance(0, b"abcd", 4), Some(4));
        assert_eq!(advance(0, b"abcde", 4), None);
        assert_eq!(advance(0, b"abcd\nabcd", 4), Some(4));
        assert_eq!(advance(0, b"abcd\n", 4), Some(0));
        assert_eq!(advance(3, b"ab", 4), None);
        assert_eq!(advance(3, b"a\nabcd", 4), Some(4));
    }

    /// PLUG-3 / TASK-94.2: 改行なしで上限を超える入力は InvalidData で打ち切られる。
    #[tokio::test]
    async fn plug3_reader_errors_on_oversized_line() {
        let data = [b'x'; 11];
        let mut r = LimitedReader::new(&data[..], 10);
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// PLUG-3 / TASK-94.2: 各行が上限以内なら全体が長くても最後まで読める。
    #[tokio::test]
    async fn plug3_reader_passes_many_short_lines() {
        let data = b"aaaa\nbbbb\ncccc\n".repeat(100);
        let mut r = LimitedReader::new(&data[..], 4);
        let mut out = Vec::new();
        let n = r.read_to_end(&mut out).await.unwrap();
        assert_eq!(n, data.len());
        assert_eq!(out, data);
    }
}
