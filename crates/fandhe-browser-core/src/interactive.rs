//! 操作モードの確定結果の部品（TASK-124・`AISNAP-20`・`AISNAP-21`。設計 `docs/design/servo-host-design.md` §14）。
//!
//! 役割: Servo ホストの DOM の取得結果（[`HostDomSnapshot`]。render crate の `HostRenderer` が作る）から、
//! 本体が保存する確定結果（[`InteractiveDocument`]）を組み立てる。番号（`data-fandhe-n`）の検査と外し、
//! 「本体の文書順の要素 → ホストの番号」の対応表、要素数・深さ・長さの上限をここで扱う（§14.3・確定条件 1・4）。
//!
//! - HTML はスクリプトが有効な扱いでパースする（ホストと同じく `noscript` の中身を生のテキストにする。§14.3）。
//!   ここでパースした [`Document`] をそのまま保持し、対応表はその [`NodeId`] に結びつける。再パースすると
//!   要素の並びが変わりうるため、利用側（ai・cdp）は保持した文書を使う
//! - 番号は先頭 0 なしの十進で、0 以上 `element_count` 未満・重複なし。違反は同期失敗（[`SyncError`]）。
//!   番号の無い要素（パーサーが補った要素）は対応表に載らず、操作の対象にならない
//! - スクリプトは実行しない。パースと直列化だけを行い、遷移・`meta refresh` を起こす経路は無い（試作 (12)）
//! - 伏せる値の第 2 層（ホストが伏せずに返した値を本体でも消す）は、ここでは行わない（TASK-125。
//!   どの層で CDP に渡る前に消すかは利用者の判断待ち。PR の前提を参照）

use std::collections::HashMap;
use std::fmt;

use crate::dom::{Document, NodeId};
use crate::parse::{ParseOptions, parse_document};

/// `dom_snapshot` の HTML の上限（UTF-8 のバイト。8 MiB。`RENDER-11`）。
pub const MAX_INTERACTIVE_HTML_BYTES: usize = 8 * 1024 * 1024;
/// 要素数の上限（`RENDER-11`）。
pub const MAX_INTERACTIVE_ELEMENTS: usize = 200_000;
/// 深さの上限（`RENDER-11`）。
pub const MAX_INTERACTIVE_DEPTH: usize = 512;
/// URL の上限（バイト）。
pub const MAX_INTERACTIVE_URL_BYTES: usize = 2048;
/// `session`・`snapshot` の上限（バイト）。
pub const MAX_INTERACTIVE_ID_BYTES: usize = 64;

const NUMBER_ATTR: &str = "data-fandhe-n";

/// ページ識別子（ホストの起動ごとに変わる `session` と文書番号 `doc_seq`。§14.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InteractivePageId {
    session: String,
    doc_seq: u64,
}

impl InteractivePageId {
    /// 作る（`session` は 1〜64 バイト）。
    pub fn new(session: &str, doc_seq: u64) -> Result<Self, SyncError> {
        if session.is_empty() || session.len() > MAX_INTERACTIVE_ID_BYTES {
            return Err(SyncError::Malformed);
        }
        Ok(Self {
            session: session.to_owned(),
            doc_seq,
        })
    }
    /// ホストの起動ごとに変わる値。
    pub fn session(&self) -> &str {
        &self.session
    }
    /// 文書番号。
    pub fn doc_seq(&self) -> u64 {
        self.doc_seq
    }
}

/// ホストの DOM の取得結果（render の `HostRenderer` が作り、[`build_interactive_document`] へ渡す）。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostDomSnapshot {
    page: InteractivePageId,
    url: String,
    html: String,
    element_count: u64,
    snapshot: String,
}

impl fmt::Debug for HostDomSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // URL・HTML はページの内容（入力値を含みうる）なので長さだけを出す。
        f.debug_struct("HostDomSnapshot")
            .field("page", &self.page)
            .field("url_len", &self.url.len())
            .field("html_len", &self.html.len())
            .field("element_count", &self.element_count)
            .finish_non_exhaustive()
    }
}

impl HostDomSnapshot {
    /// 作る。URL（http / https・2048 バイト以下）・HTML（8 MiB 以下）・`snapshot`（1〜64 バイト）を検査する
    /// （確定条件 4。ホストの応答は untrusted）。
    pub fn new(
        page: InteractivePageId,
        url: &str,
        html: String,
        element_count: u64,
        snapshot: &str,
    ) -> Result<Self, SyncError> {
        let lower: String = url.chars().take(8).collect::<String>().to_ascii_lowercase();
        if url.len() > MAX_INTERACTIVE_URL_BYTES
            || !(lower.starts_with("http://") || lower.starts_with("https://"))
        {
            return Err(SyncError::BadUrl);
        }
        if html.len() > MAX_INTERACTIVE_HTML_BYTES {
            return Err(SyncError::TooLarge);
        }
        if snapshot.is_empty() || snapshot.len() > MAX_INTERACTIVE_ID_BYTES {
            return Err(SyncError::Malformed);
        }
        Ok(Self {
            page,
            url: url.to_owned(),
            html,
            element_count,
            snapshot: snapshot.to_owned(),
        })
    }
}

/// 同期の失敗（`AISNAP-20`「同期に失敗したら古い内容を返さずエラー」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyncError {
    /// HTML が 8 MiB を超える。
    TooLarge,
    /// 要素数が 200,000 を超える。
    TooManyElements,
    /// 深さが 512 を超える。
    TooDeep,
    /// 番号の形・範囲が不正（先頭 0・十進以外・`element_count` 以上）。
    BadNumber,
    /// 同じ番号が 2 つ以上ある。
    DuplicateNumber,
    /// URL が http / https でない・長すぎる。
    BadUrl,
    /// その他の形の不正（パース・直列化の失敗を含む）。
    Malformed,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "interactive document is too large",
            Self::TooManyElements => "interactive document has too many elements",
            Self::TooDeep => "interactive document is too deep",
            Self::BadNumber => "interactive document has an invalid element number",
            Self::DuplicateNumber => "interactive document has a duplicated element number",
            Self::BadUrl => "interactive document url is not allowed",
            Self::Malformed => "interactive document is malformed",
        })
    }
}

impl std::error::Error for SyncError {}

/// 操作モードの確定結果の中身（§14.3・§14.4）。番号を外した文書と、要素 → ホストの番号の対応表を持つ。
#[non_exhaustive]
pub struct InteractiveDocument {
    page: InteractivePageId,
    revision: u64,
    snapshot: String,
    document: Document,
    numbers: HashMap<NodeId, u32>,
}

impl fmt::Debug for InteractiveDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractiveDocument")
            .field("page", &self.page)
            .field("revision", &self.revision)
            .field("numbered", &self.numbers.len())
            .finish_non_exhaustive()
    }
}

impl InteractiveDocument {
    /// 取得した時点のページ識別子。
    pub fn page(&self) -> &InteractivePageId {
        &self.page
    }
    /// 版（同じナビゲーション世代の中で、同期のたびに 1 上がる。最初は 1）。
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// ホストの取得の識別子（`act` に付ける）。
    pub fn snapshot(&self) -> &str {
        &self.snapshot
    }
    /// 番号を外した文書（スクリプトが有効な扱いでパース済み。利用側は再パースせずこれを使う）。
    pub fn document(&self) -> &Document {
        &self.document
    }
    /// 要素に対応するホストの番号。番号の無い要素は `None`（操作の対象にしない）。
    pub fn host_number(&self, element: NodeId) -> Option<u32> {
        self.numbers.get(&element).copied()
    }

    pub(crate) fn with_revision(mut self, revision: u64) -> Self {
        self.revision = revision;
        self
    }
}

/// 検査済みの同期の結果（[`InteractiveDocument`] と、保存用の番号なし HTML・URL）。`Debug` は長さだけを出す。
#[non_exhaustive]
pub struct InteractiveSync {
    pub(crate) url: String,
    pub(crate) html: String,
    pub(crate) document: InteractiveDocument,
}

impl fmt::Debug for InteractiveSync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractiveSync")
            .field("url_len", &self.url.len())
            .field("html_len", &self.html.len())
            .field("document", &self.document)
            .finish()
    }
}

impl InteractiveSync {
    /// 取得した時点のページ識別子。
    pub fn page(&self) -> &InteractivePageId {
        self.document.page()
    }
}

/// ホストの取得結果を検査し、番号を外した文書と対応表を作る（§14.3・確定条件 1・4）。
pub fn build_interactive_document(snapshot: HostDomSnapshot) -> Result<InteractiveSync, SyncError> {
    let options = ParseOptions::default().with_scripting_enabled(true);
    let mut document = parse_document(&snapshot.html, &options)
        .map_err(|_| SyncError::Malformed)?
        .document;
    let elements: Vec<NodeId> = document
        .descendants(document.root())
        .filter(|id| document.local_name(*id).is_some())
        .collect();
    if elements.len() > MAX_INTERACTIVE_ELEMENTS {
        return Err(SyncError::TooManyElements);
    }
    // ホストは `template` の中身を出さない（§14.3）。中身があれば応答の不正として扱う（`descendants` は
    // 中身を巡回しないが直列化は出力するため、番号や上限の検査を通らない要素が保存 HTML に残るのを防ぐ）。
    if elements.iter().any(|&id| {
        document
            .template_contents(id)
            .is_some_and(|c| document.children(c).next().is_some())
    }) {
        return Err(SyncError::Malformed);
    }
    let mut depth: HashMap<NodeId, usize> = HashMap::with_capacity(elements.len());
    let mut numbers: HashMap<NodeId, u32> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for &id in &elements {
        let d = document
            .parent(id)
            .and_then(|p| depth.get(&p).copied())
            .unwrap_or(0)
            .saturating_add(1);
        if d > MAX_INTERACTIVE_DEPTH {
            return Err(SyncError::TooDeep);
        }
        depth.insert(id, d);
        if let Some(raw) = document.attribute(id, NUMBER_ATTR) {
            let n = parse_number(raw, snapshot.element_count)?;
            if !seen.insert(n) {
                return Err(SyncError::DuplicateNumber);
            }
            numbers.insert(id, n);
        }
    }
    for &id in numbers.keys() {
        document
            .remove_attribute(id, NUMBER_ATTR)
            .map_err(|_| SyncError::Malformed)?;
    }
    let html = document
        .serialize_html()
        .map_err(|_| SyncError::Malformed)?
        .into_html();
    Ok(InteractiveSync {
        url: snapshot.url,
        html,
        document: InteractiveDocument {
            page: snapshot.page,
            revision: 1,
            snapshot: snapshot.snapshot,
            document,
            numbers,
        },
    })
}

/// 先頭 0 なしの十進（`"0"` は可）で、`element_count` 未満の番号。
fn parse_number(raw: &str, element_count: u64) -> Result<u32, SyncError> {
    let ok = !raw.is_empty()
        && raw.bytes().all(|b| b.is_ascii_digit())
        && (raw == "0" || !raw.starts_with('0'));
    if !ok {
        return Err(SyncError::BadNumber);
    }
    let n: u32 = raw.parse().map_err(|_| SyncError::BadNumber)?;
    if u64::from(n) >= element_count || n as usize >= MAX_INTERACTIVE_ELEMENTS {
        return Err(SyncError::BadNumber);
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(html: &str, count: u64) -> HostDomSnapshot {
        HostDomSnapshot::new(
            InteractivePageId::new("s", 1).unwrap(),
            "https://example.com/",
            html.to_owned(),
            count,
            "s1",
        )
        .unwrap()
    }

    /// AISNAP-20・確定条件 1: 番号を外した HTML を保存し、要素 → 番号の対応表を作る。番号の無い要素は載らない。
    #[test]
    fn aisnap20_builds_table_and_strips_numbers() {
        let html = r#"<html data-fandhe-n="0"><head data-fandhe-n="1"></head><body data-fandhe-n="2"><p data-fandhe-n="3">x</p><table data-fandhe-n="4"><tr data-fandhe-n="5"><td data-fandhe-n="6">c</td></tr></table></body></html>"#;
        let s = build_interactive_document(snap(html, 7)).unwrap();
        assert!(!s.html.contains("data-fandhe-n"), "{}", s.html);
        let doc = &s.document;
        assert_eq!(doc.revision(), 1);
        assert_eq!(doc.snapshot(), "s1");
        let d = doc.document();
        let by_tag = |tag: &str| {
            d.descendants(d.root())
                .find(|id| d.local_name(*id) == Some(tag))
                .unwrap()
        };
        assert_eq!(doc.host_number(by_tag("p")), Some(3));
        assert_eq!(doc.host_number(by_tag("td")), Some(6));
        // パーサーが補った tbody には番号が無い。
        assert_eq!(doc.host_number(by_tag("tbody")), None);
        assert!(d.attribute(by_tag("p"), NUMBER_ATTR).is_none());
    }

    /// 確定条件 1: 番号の形・範囲・重複の違反は同期失敗。
    #[test]
    fn aisnap20_rejects_bad_numbers() {
        for (html, count, want) in [
            (r#"<p data-fandhe-n="01">a</p>"#, 10, SyncError::BadNumber),
            (r#"<p data-fandhe-n="-1">a</p>"#, 10, SyncError::BadNumber),
            (r#"<p data-fandhe-n="1e1">a</p>"#, 10, SyncError::BadNumber),
            (r#"<p data-fandhe-n="10">a</p>"#, 10, SyncError::BadNumber),
            (r#"<p data-fandhe-n="">a</p>"#, 10, SyncError::BadNumber),
            (
                r#"<p data-fandhe-n="3">a</p><p data-fandhe-n="3">b</p>"#,
                10,
                SyncError::DuplicateNumber,
            ),
        ] {
            assert_eq!(
                build_interactive_document(snap(html, count)).err(),
                Some(want),
                "{html}"
            );
        }
    }

    /// RENDER-11: 深さ 512 を超える文書は同期失敗。noscript の中身はスクリプトが有効な扱い（生のテキスト）。
    #[test]
    fn render11_depth_limit_and_noscript_as_text() {
        let deep = "<div>".repeat(MAX_INTERACTIVE_DEPTH + 2);
        assert_eq!(
            build_interactive_document(snap(&deep, 0)).err(),
            Some(SyncError::TooDeep)
        );
        // noscript の中に仕込んだ同名の属性は要素にならず、番号にならない。
        let html =
            r#"<body data-fandhe-n="0"><noscript><p data-fandhe-n="0">x</p></noscript></body>"#;
        let s = build_interactive_document(snap(html, 1)).unwrap();
        let d = s.document.document();
        assert!(
            d.descendants(d.root())
                .all(|id| d.local_name(id) != Some("p"))
        );
    }

    /// 確定条件 4: URL の scheme・長さ、HTML の長さ、snapshot を作る時点で検査する。
    #[test]
    fn host_snapshot_validation() {
        let page = || InteractivePageId::new("s", 1).unwrap();
        assert_eq!(
            HostDomSnapshot::new(page(), "about:blank", String::new(), 0, "s1").err(),
            Some(SyncError::BadUrl)
        );
        assert_eq!(
            HostDomSnapshot::new(
                page(),
                "https://e/",
                "a".repeat(MAX_INTERACTIVE_HTML_BYTES + 1),
                0,
                "s1"
            )
            .err(),
            Some(SyncError::TooLarge)
        );
        assert_eq!(
            HostDomSnapshot::new(page(), "https://e/", String::new(), 0, "").err(),
            Some(SyncError::Malformed)
        );
        assert!(InteractivePageId::new("", 0).is_err());
        let s = HostDomSnapshot::new(page(), "https://secret.example/?q=1", "pw".into(), 0, "s1")
            .unwrap();
        assert!(!format!("{s:?}").contains("secret"));
    }

    /// template の中身に要素があれば同期失敗（番号・上限の検査を迂回させない）。要素数の上限と深さの境界。
    #[test]
    fn render11_template_contents_and_limits() {
        let html = r#"<body data-fandhe-n="0"><template data-fandhe-n="1"><p data-fandhe-n="01">x</p></template></body>"#;
        assert_eq!(
            build_interactive_document(snap(html, 2)).err(),
            Some(SyncError::Malformed)
        );
        let empty = r#"<body data-fandhe-n="0"><template data-fandhe-n="1"></template></body>"#;
        assert!(build_interactive_document(snap(empty, 2)).is_ok());
        let many = "<i></i>".repeat(MAX_INTERACTIVE_ELEMENTS);
        assert_eq!(
            build_interactive_document(snap(&many, 0)).err(),
            Some(SyncError::TooManyElements)
        );
        // 祖先の html・body の 2 段を含めて深さちょうど 512 は通る。
        let ok = "<div>".repeat(MAX_INTERACTIVE_DEPTH - 2);
        assert!(build_interactive_document(snap(&ok, 0)).is_ok());
        let s = build_interactive_document(snap("<p>secret-text</p>", 0)).unwrap();
        assert!(!format!("{s:?}").contains("secret-text"));
    }

    #[test]
    fn interactive_document_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InteractiveDocument>();
    }
}
