//! ホストの stderr 末尾の上限付きバッファと伏字化（設計 §2.6。TASK-38・#994）。
//!
//! stderr 専用スレッドが [`StderrTail::push`] で書き込み、[`super::HostProcess::diagnostics`]
//! が [`StderrTail::redacted`] で読む。ホスト由来の出力は untrusted で、URL のクエリや
//! userinfo を含みうるため、診断経路でも伏字化してから返す（多層防御）。

/// 保持する末尾のバイト数（8 KiB。設計 §2.6）。
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// 末尾 [`STDERR_TAIL_BYTES`] バイトだけを残すバッファ。
#[derive(Debug, Default)]
pub struct StderrTail {
    buf: Vec<u8>,
    /// 先頭側を捨てたか。捨てた場合、先頭の語は途中で切れており伏字化できないため破棄する。
    truncated: bool,
}

impl StderrTail {
    /// 空のバッファを作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// バイト列を追加し、上限を超えた先頭側を捨てる。
    pub fn push(&mut self, data: &[u8]) {
        // 巨大な入力でも確保が上限を大きく超えないよう、末尾だけを取り込む。
        let start = data.len().saturating_sub(STDERR_TAIL_BYTES);
        if start > 0 {
            self.truncated = true;
        }
        if let Some(tail) = data.get(start..) {
            self.buf.extend_from_slice(tail);
        }
        if self.buf.len() > STDERR_TAIL_BYTES {
            self.truncated = true;
            let excess = self.buf.len() - STDERR_TAIL_BYTES;
            self.buf.drain(..excess);
        }
    }

    /// 保持しているバイト列（テスト用）。
    #[cfg(test)]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// 伏字化した文字列。語中の `?` 以降を `?<redacted>` にし、
    /// `scheme://userinfo@` の userinfo を除く。
    ///
    /// 先頭を切り捨てている場合は、切断位置にかかった先頭の語（`scheme://` や `?` を欠いて
    /// 機密部分だけが残りうる）を空白まで破棄してから伏字化する（設計 §2.6）。
    pub fn redacted(&self) -> String {
        let text = String::from_utf8_lossy(&self.buf);
        if self.truncated {
            let rest = text
                .find(char::is_whitespace)
                .and_then(|i| text.get(i..))
                .unwrap_or("");
            redact(rest)
        } else {
            redact(&text)
        }
    }
}

fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            out.push_str(&redact_word(&word));
            word.clear();
            out.push(c);
        } else {
            word.push(c);
        }
    }
    out.push_str(&redact_word(&word));
    out
}

fn redact_word(word: &str) -> String {
    let mut w = word.to_string();
    if let Some(scheme_end) = w.find("://") {
        let rest_start = scheme_end + 3;
        let rest = w.get(rest_start..).unwrap_or("");
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        if let Some(at) = rest.get(..authority_end).and_then(|a| a.rfind('@')) {
            let keep = rest.get(at + 1..).unwrap_or("").to_string();
            w = format!("{}{}", w.get(..rest_start).unwrap_or(""), keep);
        }
    }
    if let Some(q) = w.find('?') {
        w.truncate(q);
        w.push_str("?<redacted>");
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render3_keeps_only_last_8192_bytes() {
        let mut t = StderrTail::new();
        let mut data = vec![b'a'; 10_000];
        data.extend(vec![b'b'; 100]);
        t.push(&data);
        let mut expect = vec![b'a'; 8092];
        expect.extend(vec![b'b'; 100]);
        assert_eq!(t.as_bytes(), expect.as_slice());
        t.push(b"zz");
        assert_eq!(t.as_bytes().len(), 8192);
        assert!(t.as_bytes().ends_with(b"bbzz"));
    }

    #[test]
    fn render3_redacts_query_and_userinfo() {
        let mut t = StderrTail::new();
        t.push(b"load https://a.example/b?token=x&y=1 failed\nnext https://u:p@h.example/p#f ok\n");
        assert_eq!(
            t.redacted(),
            "load https://a.example/b?<redacted> failed\nnext https://h.example/p#f ok\n"
        );
    }

    #[test]
    fn render3_drops_cut_leading_word_so_query_secret_never_leaks() {
        // 切断位置が URL の `?` の直後に来るように詰める。
        let head = "https://h.example/p?";
        let mut tail = String::from("token=SECRET123 rest\n");
        tail.push_str(&" ".repeat(STDERR_TAIL_BYTES - tail.len()));
        let expect = format!(" rest\n{}", " ".repeat(STDERR_TAIL_BYTES - 21));
        let mut data = vec![b'a'; 50];
        data.extend_from_slice(head.as_bytes());
        data.extend_from_slice(tail.as_bytes());

        // 1 回の push（入力側の切り捨て）。
        let mut t = StderrTail::new();
        t.push(&data);
        assert_eq!(t.as_bytes().len(), STDERR_TAIL_BYTES);
        assert!(t.as_bytes().starts_with(b"token=SECRET123"));
        assert_eq!(t.redacted(), expect);

        // 2 回の push（drain による切り捨て）。
        let mut t = StderrTail::new();
        let (a, b) = data.split_at(100);
        t.push(a);
        t.push(b);
        assert_eq!(t.redacted(), expect);
    }

    #[test]
    fn render3_untruncated_buffer_keeps_first_word() {
        let mut t = StderrTail::new();
        t.push(b"first https://h.example/p?token=SECRET123\n");
        assert_eq!(t.redacted(), "first https://h.example/p?<redacted>\n");
    }
}
