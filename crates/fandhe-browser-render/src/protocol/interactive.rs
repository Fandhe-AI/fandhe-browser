//! 操作モードのメソッド（`dom_snapshot`・`act`・`wait`）のクライアント側型（RENDER-10・RENDER-11・
//! TASK-123。設計 `docs/design/servo-host-design.md` §14）。
//!
//! 親モジュール [`crate::protocol`] と同じ方針で、要求は検証付きコンストラクタでだけ作り、応答は
//! untrusted としてスキーマ相当の検証を fail-closed で行う。契約の正本はスキーマと
//! `tests/fixtures/protocol/interactive/` の fixture。
//!
//! 本体側の検査（要素数 200,000・深さ 512・番号の一意性。§14.3）は HTML をパースする側
//! （TASK-124・TASK-125）が行う。ここでは長さ・URL の scheme・数の範囲など、応答の形だけを見る。
//! `act` の値（`fill` の `text` 等）は `Debug` に出さない（入力値をログ・エラーに出さない。AISNAP 共通条件）。

use std::fmt;

use serde_json::{Map, Value, json};

use super::{
    MAX_REQUEST_ID, MAX_SELECTOR_BYTES, MAX_URL_BYTES, ProtocolError, bad, str_field, uint_field,
};

/// `dom_snapshot` の HTML の上限（UTF-8 のバイト。8 MiB。`RENDER-11`）。
pub const MAX_DOM_SNAPSHOT_HTML_BYTES: usize = 8_388_608;
/// `page.session`・`snapshot` の上限（文字列の長さ）。
pub const MAX_SESSION_BYTES: usize = 64;
/// `act` の `node` の最大値（要素数の上限 200,000 未満）。
pub const MAX_NODE: u32 = 199_999;
/// 照合値の `tag` の上限。
pub const MAX_EXPECT_TAG_BYTES: usize = 64;
/// 照合値の `id`・`name`・`type` の上限（超える属性は送らない。§14.4）。
pub const MAX_EXPECT_ATTR_BYTES: usize = 256;
/// `fill` の `text` の上限。
pub const MAX_FILL_TEXT_BYTES: usize = 8192;
/// `select` の `values` の件数の上限。
pub const MAX_SELECT_VALUES: usize = 32;
/// `select` の値 1 件の上限。
pub const MAX_SELECT_VALUE_BYTES: usize = 512;
/// `press` の `key` の上限。
pub const MAX_KEY_BYTES: usize = 32;
/// `wait` の条件の文字列（セレクタ・テキスト）の上限。
pub const MAX_WAIT_CONDITION_BYTES: usize = MAX_SELECTOR_BYTES;
/// `wait` の `timeout_ms` の上限（`AISNAP-18`）。
pub const MAX_WAIT_TIMEOUT_MS: u32 = 60_000;

const MAX_COUNT: u64 = MAX_REQUEST_ID;

/// ページ識別子（ホストの起動ごとに変わる `session` と文書番号 `doc_seq`。§14.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageInfo {
    session: String,
    doc_seq: u64,
}

impl PageInfo {
    /// 作る（`session` は 1〜64 バイト、`doc_seq` は 2^53-1 以下）。
    pub fn new(session: &str, doc_seq: u64) -> Result<Self, ProtocolError> {
        if session.is_empty() || session.len() > MAX_SESSION_BYTES {
            return Err(ProtocolError::InvalidParams(
                "page session length out of range",
            ));
        }
        if doc_seq > MAX_REQUEST_ID {
            return Err(ProtocolError::InvalidParams("page doc_seq out of range"));
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

    pub(super) fn to_json(&self) -> Value {
        json!({ "session": self.session, "doc_seq": self.doc_seq })
    }

    pub(super) fn from_json(v: &Value) -> Result<Self, ProtocolError> {
        let obj = v.as_object().ok_or(bad("page is not an object"))?;
        let session = str_field(obj, "session", MAX_SESSION_BYTES, "invalid page session")?;
        if session.is_empty() {
            return Err(bad("invalid page session"));
        }
        let doc_seq = uint_field(obj, "doc_seq", "invalid page doc_seq")?;
        if doc_seq > MAX_REQUEST_ID {
            return Err(bad("invalid page doc_seq"));
        }
        Ok(Self {
            session: session.to_owned(),
            doc_seq,
        })
    }
}

/// `dom_snapshot` の結果（§14.3）。HTML は番号（`data-fandhe-n`）つきで、外して対応表にするのは本体。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DomSnapshotResult {
    page: PageInfo,
    url: String,
    html: String,
    element_count: u64,
    snapshot: String,
    iframes: u64,
    shadow_roots: u64,
    redacted: u64,
}

impl fmt::Debug for DomSnapshotResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // URL・HTML はページの内容（入力値を含みうる）なので、長さだけを出す。
        f.debug_struct("DomSnapshotResult")
            .field("page", &self.page)
            .field("url_len", &self.url.len())
            .field("html_len", &self.html.len())
            .field("element_count", &self.element_count)
            .field("snapshot", &self.snapshot)
            .finish_non_exhaustive()
    }
}

impl DomSnapshotResult {
    /// 取得した時点のページ識別子。保存済みの値と違えば文書が変わっている。
    pub fn page(&self) -> &PageInfo {
        &self.page
    }
    /// 現在の URL（http / https・2048 バイト以下に検証済み）。
    pub fn url(&self) -> &str {
        &self.url
    }
    /// 番号つきの HTML（8 MiB 以下に検証済み）。
    pub fn html(&self) -> &str {
        &self.html
    }
    /// ホストが番号を振った要素の数。
    pub fn element_count(&self) -> u64 {
        self.element_count
    }
    /// この取得の識別子（`act` に付ける）。
    pub fn snapshot(&self) -> &str {
        &self.snapshot
    }
    /// 中身を含めなかった iframe の数。
    pub fn iframes(&self) -> u64 {
        self.iframes
    }
    /// 中身を含めなかった shadow root の数。
    pub fn shadow_roots(&self) -> u64 {
        self.shadow_roots
    }
    /// 値を伏せた要素の数（キーが無ければ 0）。
    pub fn redacted(&self) -> u64 {
        self.redacted
    }
}

/// 照合値（§14.4）。`tag` は必須、`id`・`name`・`type` は送ったものだけをホストが照合する。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActExpect {
    tag: String,
    id: Option<String>,
    name: Option<String>,
    kind: Option<String>,
}

impl fmt::Debug for ActExpect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActExpect")
            .field("tag", &self.tag)
            .finish_non_exhaustive()
    }
}

impl ActExpect {
    /// `tag`（小文字のローカル名）だけの照合値。
    pub fn new(tag: &str) -> Result<Self, ProtocolError> {
        if tag.is_empty() || tag.len() > MAX_EXPECT_TAG_BYTES {
            return Err(ProtocolError::InvalidParams(
                "expect tag length out of range",
            ));
        }
        Ok(Self {
            tag: tag.to_owned(),
            id: None,
            name: None,
            kind: None,
        })
    }

    /// `id` 属性も照合する。256 バイトを超える値は照合に使わない（呼び出し側が送らない。§14.4）。
    pub fn with_id(mut self, v: &str) -> Result<Self, ProtocolError> {
        self.id = Some(attr(v)?);
        Ok(self)
    }
    /// `name` 属性も照合する。
    pub fn with_name(mut self, v: &str) -> Result<Self, ProtocolError> {
        self.name = Some(attr(v)?);
        Ok(self)
    }
    /// `type` 属性も照合する。
    pub fn with_type(mut self, v: &str) -> Result<Self, ProtocolError> {
        self.kind = Some(attr(v)?);
        Ok(self)
    }

    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("tag".into(), json!(self.tag));
        for (k, v) in [("id", &self.id), ("name", &self.name), ("type", &self.kind)] {
            if let Some(v) = v {
                m.insert(k.into(), json!(v));
            }
        }
        Value::Object(m)
    }
}

fn attr(v: &str) -> Result<String, ProtocolError> {
    if v.len() > MAX_EXPECT_ATTR_BYTES {
        return Err(ProtocolError::InvalidParams("expect attribute too long"));
    }
    Ok(v.to_owned())
}

/// `act` の操作（`RENDER-10`）。値は `Debug` に出さない。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// クリック。
    Click,
    /// 文字の入力。`sensitive` は本体が伏せる対象に加えるための印。
    Fill {
        /// 入力する文字列（8192 バイト以下）。
        text: String,
        /// 伏せる対象として扱うか。
        sensitive: bool,
    },
    /// `select` の選択（`option` の `value`）。
    Select {
        /// 選ぶ値（1〜32 件・各 512 バイト以下）。
        values: Vec<String>,
    },
    /// キーの押下（キー名か 1 文字。一覧はホストが検査する）。
    Press {
        /// キー（1〜32 バイト）。
        key: String,
    },
}

impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Click => "Click",
            Self::Fill { .. } => "Fill",
            Self::Select { .. } => "Select",
            Self::Press { .. } => "Press",
        })
    }
}

impl Action {
    /// `fill`（長さを検証する）。
    pub fn fill(text: &str, sensitive: bool) -> Result<Self, ProtocolError> {
        if text.len() > MAX_FILL_TEXT_BYTES {
            return Err(ProtocolError::InvalidParams("fill text too long"));
        }
        Ok(Self::Fill {
            text: text.to_owned(),
            sensitive,
        })
    }

    /// `select`（件数と長さを検証する）。
    pub fn select(values: &[&str]) -> Result<Self, ProtocolError> {
        if values.is_empty() || values.len() > MAX_SELECT_VALUES {
            return Err(ProtocolError::InvalidParams(
                "select values count out of range",
            ));
        }
        if values.iter().any(|v| v.len() > MAX_SELECT_VALUE_BYTES) {
            return Err(ProtocolError::InvalidParams("select value too long"));
        }
        Ok(Self::Select {
            values: values.iter().map(|v| (*v).to_owned()).collect(),
        })
    }

    /// `press`（長さを検証する）。
    pub fn press(key: &str) -> Result<Self, ProtocolError> {
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Err(ProtocolError::InvalidParams(
                "press key length out of range",
            ));
        }
        Ok(Self::Press {
            key: key.to_owned(),
        })
    }

    /// 上限を検査する（variant を直接組んだ値も、送信前にここで拒否する）。
    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Click => Ok(()),
            Self::Fill { text, .. } if text.len() > MAX_FILL_TEXT_BYTES => {
                Err(ProtocolError::InvalidParams("fill text too long"))
            }
            Self::Select { values }
                if values.is_empty()
                    || values.len() > MAX_SELECT_VALUES
                    || values.iter().any(|v| v.len() > MAX_SELECT_VALUE_BYTES) =>
            {
                Err(ProtocolError::InvalidParams("select values out of range"))
            }
            Self::Press { key } if key.is_empty() || key.len() > MAX_KEY_BYTES => Err(
                ProtocolError::InvalidParams("press key length out of range"),
            ),
            _ => Ok(()),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Click => json!({ "kind": "click" }),
            Self::Fill { text, sensitive } => {
                json!({ "kind": "fill", "text": text, "sensitive": sensitive })
            }
            Self::Select { values } => json!({ "kind": "select", "values": values }),
            Self::Press { key } => json!({ "kind": "press", "key": key }),
        }
    }
}

/// `act` のパラメータ（§14.4）。セレクタは持たない（本体が写しの上で解決し番号で送る。確定条件 8）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActParams {
    page: PageInfo,
    snapshot: String,
    node: u32,
    expect: ActExpect,
    action: Action,
}

impl ActParams {
    /// 作る。`snapshot` は `dom_snapshot` の結果の値、`node` は番号（199,999 以下）。
    pub fn new(
        page: PageInfo,
        snapshot: &str,
        node: u32,
        expect: ActExpect,
        action: Action,
    ) -> Result<Self, ProtocolError> {
        if snapshot.is_empty() || snapshot.len() > MAX_SESSION_BYTES {
            return Err(ProtocolError::InvalidParams("snapshot length out of range"));
        }
        if node > MAX_NODE {
            return Err(ProtocolError::InvalidParams("node out of range"));
        }
        action.validate()?;
        Ok(Self {
            page,
            snapshot: snapshot.to_owned(),
            node,
            expect,
            action,
        })
    }

    /// 操作。
    pub fn action(&self) -> &Action {
        &self.action
    }

    pub(super) fn to_json(&self) -> Value {
        json!({
            "page": self.page.to_json(),
            "snapshot": self.snapshot,
            "node": self.node,
            "expect": self.expect.to_json(),
            "action": self.action.to_json(),
        })
    }
}

/// `act` の結果。遷移先の URL は返らない（クエリを応答に出さないため。§14.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActResult {
    page: PageInfo,
    navigated: bool,
}

impl ActResult {
    /// 操作の後のページ識別子。
    pub fn page(&self) -> &PageInfo {
        &self.page
    }
    /// 文書番号が変わったか。
    pub fn navigated(&self) -> bool {
        self.navigated
    }
}

/// `wait` の条件（`AISNAP-18`）。文字列はセレクタ・テキストとも 1〜1024 バイト。`Debug` に文字列は出さない。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WaitCondition {
    /// セレクタに一致する要素が現れる。
    SelectorPresent(String),
    /// セレクタに一致する要素が無くなる。
    SelectorAbsent(String),
    /// テキストが現れる。
    TextPresent(String),
    /// 読み込みの完了。
    LoadComplete,
}

impl fmt::Debug for WaitCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SelectorPresent(_) => "SelectorPresent",
            Self::SelectorAbsent(_) => "SelectorAbsent",
            Self::TextPresent(_) => "TextPresent",
            Self::LoadComplete => "LoadComplete",
        })
    }
}

impl WaitCondition {
    fn to_json(&self) -> Value {
        match self {
            Self::SelectorPresent(s) => json!({ "kind": "selector_present", "selector": s }),
            Self::SelectorAbsent(s) => json!({ "kind": "selector_absent", "selector": s }),
            Self::TextPresent(t) => json!({ "kind": "text_present", "text": t }),
            Self::LoadComplete => json!({ "kind": "load_complete" }),
        }
    }

    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::SelectorPresent(s) | Self::SelectorAbsent(s) | Self::TextPresent(s)
                if s.is_empty() || s.len() > MAX_WAIT_CONDITION_BYTES =>
            {
                Err(ProtocolError::InvalidParams(
                    "wait condition length out of range",
                ))
            }
            _ => Ok(()),
        }
    }
}

/// `wait` のパラメータ（§14.5）。`timeout_ms` は必須で 1〜60,000。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WaitParams {
    page: PageInfo,
    condition: WaitCondition,
    timeout_ms: u32,
}

impl WaitParams {
    /// 作る（条件の長さと上限時間を検証する）。
    pub fn new(
        page: PageInfo,
        condition: WaitCondition,
        timeout_ms: u32,
    ) -> Result<Self, ProtocolError> {
        condition.validate()?;
        if timeout_ms == 0 || timeout_ms > MAX_WAIT_TIMEOUT_MS {
            return Err(ProtocolError::InvalidParams("wait timeout_ms out of range"));
        }
        Ok(Self {
            page,
            condition,
            timeout_ms,
        })
    }

    /// 上限時間（ミリ秒）。
    pub fn timeout_ms(&self) -> u32 {
        self.timeout_ms
    }

    pub(super) fn to_json(&self) -> Value {
        json!({
            "page": self.page.to_json(),
            "condition": self.condition.to_json(),
            "timeout_ms": self.timeout_ms,
        })
    }
}

/// `wait` の結果（条件が成立した時点のページ識別子）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WaitResult {
    page: PageInfo,
}

impl WaitResult {
    /// 条件が成立した時点のページ識別子。
    pub fn page(&self) -> &PageInfo {
        &self.page
    }
}

fn count_field(obj: &Map<String, Value>, key: &str) -> Result<u64, ProtocolError> {
    uint_field(obj, key, "invalid count").and_then(|n| {
        if n <= MAX_COUNT {
            Ok(n)
        } else {
            Err(bad("invalid count"))
        }
    })
}

/// `dom_snapshot` の結果を検証する。`url` は長さと scheme（http / https）も見る（確定条件 4）。
pub(super) fn parse_dom_snapshot(
    obj: &Map<String, Value>,
) -> Result<DomSnapshotResult, ProtocolError> {
    let page = PageInfo::from_json(obj.get("page").ok_or(bad("page missing"))?)?;
    let url = str_field(obj, "url", MAX_URL_BYTES, "invalid url")?;
    let lower: String = url.chars().take(8).collect::<String>().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(bad("url scheme must be http or https"));
    }
    let html = str_field(obj, "html", MAX_DOM_SNAPSHOT_HTML_BYTES, "invalid html")?;
    let snapshot = str_field(obj, "snapshot", MAX_SESSION_BYTES, "invalid snapshot")?;
    if snapshot.is_empty() {
        return Err(bad("invalid snapshot"));
    }
    let excluded = obj
        .get("excluded")
        .and_then(Value::as_object)
        .ok_or(bad("excluded missing"))?;
    let redacted = if obj.contains_key("redacted") {
        count_field(obj, "redacted")?
    } else {
        0
    };
    Ok(DomSnapshotResult {
        page,
        url: url.to_owned(),
        html: html.to_owned(),
        element_count: count_field(obj, "element_count")?,
        snapshot: snapshot.to_owned(),
        iframes: count_field(excluded, "iframes")?,
        shadow_roots: count_field(excluded, "shadow_roots")?,
        redacted,
    })
}

pub(super) fn parse_act(obj: &Map<String, Value>) -> Result<ActResult, ProtocolError> {
    let page = PageInfo::from_json(obj.get("page").ok_or(bad("page missing"))?)?;
    let navigated = obj
        .get("navigated")
        .and_then(Value::as_bool)
        .ok_or(bad("invalid navigated"))?;
    Ok(ActResult { page, navigated })
}

pub(super) fn parse_wait(obj: &Map<String, Value>) -> Result<WaitResult, ProtocolError> {
    let page = PageInfo::from_json(obj.get("page").ok_or(bad("page missing"))?)?;
    Ok(WaitResult { page })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> PageInfo {
        PageInfo::new("0123456789abcdef0123456789abcdef", 3).unwrap()
    }

    /// RENDER-10: 要求の上限（照合値・fill・select・press・node・snapshot）を送信前に検査する。
    #[test]
    fn render10_act_params_boundaries() {
        assert!(Action::fill(&"a".repeat(MAX_FILL_TEXT_BYTES), false).is_ok());
        assert!(Action::fill(&"a".repeat(MAX_FILL_TEXT_BYTES + 1), false).is_err());
        assert!(Action::select(&[]).is_err());
        assert!(Action::select(&["x"; MAX_SELECT_VALUES]).is_ok());
        assert!(Action::select(&["x"; MAX_SELECT_VALUES + 1]).is_err());
        assert!(Action::select(&[&"v".repeat(MAX_SELECT_VALUE_BYTES + 1)]).is_err());
        assert!(Action::press("").is_err());
        assert!(Action::press(&"k".repeat(MAX_KEY_BYTES + 1)).is_err());
        assert!(ActExpect::new("").is_err());
        assert!(
            ActExpect::new("button")
                .unwrap()
                .with_id(&"i".repeat(MAX_EXPECT_ATTR_BYTES + 1))
                .is_err()
        );
        let e = ActExpect::new("button").unwrap();
        assert!(ActParams::new(page(), "s1", MAX_NODE, e.clone(), Action::Click).is_ok());
        assert!(ActParams::new(page(), "s1", MAX_NODE + 1, e.clone(), Action::Click).is_err());
        assert!(ActParams::new(page(), "", 0, e, Action::Click).is_err());
        // variant を直接組んでも ActParams::new が拒否する。
        for bad in [
            Action::Fill {
                text: "a".repeat(MAX_FILL_TEXT_BYTES + 1),
                sensitive: false,
            },
            Action::Select { values: vec![] },
            Action::Press { key: String::new() },
        ] {
            assert!(ActParams::new(page(), "s1", 0, ActExpect::new("a").unwrap(), bad).is_err());
        }
        assert!(PageInfo::new("", 0).is_err());
        assert!(PageInfo::new("s", MAX_REQUEST_ID + 1).is_err());
    }

    /// AISNAP-18: wait の条件の長さと上限時間。
    #[test]
    fn aisnap18_wait_params_boundaries() {
        let ok = WaitCondition::SelectorPresent("#a".into());
        assert!(WaitParams::new(page(), ok.clone(), 1).is_ok());
        assert!(WaitParams::new(page(), ok.clone(), MAX_WAIT_TIMEOUT_MS).is_ok());
        assert!(WaitParams::new(page(), ok.clone(), 0).is_err());
        assert!(WaitParams::new(page(), ok, MAX_WAIT_TIMEOUT_MS + 1).is_err());
        assert!(WaitParams::new(page(), WaitCondition::TextPresent(String::new()), 1).is_err());
        assert!(
            WaitParams::new(
                page(),
                WaitCondition::SelectorAbsent("a".repeat(MAX_WAIT_CONDITION_BYTES + 1)),
                1
            )
            .is_err()
        );
        assert!(WaitParams::new(page(), WaitCondition::LoadComplete, 1).is_ok());
    }

    /// 入力値（fill の text・select の値・照合値の属性）を Debug に出さない。
    #[test]
    fn debug_does_not_leak_input_values() {
        let p = ActParams::new(
            page(),
            "s1",
            1,
            ActExpect::new("input")
                .unwrap()
                .with_name("secret-name")
                .unwrap(),
            Action::fill("secret-text", true).unwrap(),
        )
        .unwrap();
        let s = format!("{p:?}");
        assert!(
            !s.contains("secret-text") && !s.contains("secret-name"),
            "{s}"
        );
        assert!(!format!("{:?}", Action::select(&["secret-v"]).unwrap()).contains("secret-v"));
    }

    /// RENDER-11: url の scheme・HTML の上限・必須の excluded を検査する。
    #[test]
    fn render11_dom_snapshot_result_validation() {
        let base = json!({
            "page": {"session": "s", "doc_seq": 1},
            "url": "https://example.com/",
            "html": "<html data-fandhe-n=\"0\"></html>",
            "element_count": 1,
            "snapshot": "s1",
            "excluded": {"iframes": 0, "shadow_roots": 0}
        });
        let parse = |v: &Value| parse_dom_snapshot(v.as_object().unwrap());
        let r = parse(&base).unwrap();
        assert_eq!(
            (r.element_count(), r.redacted(), r.snapshot()),
            (1, 0, "s1")
        );
        assert!(!format!("{r:?}").contains("example.com"));
        for (k, v) in [
            ("url", json!("about:blank")),
            ("url", json!("data:text/html,x")),
            ("html", json!("a".repeat(MAX_DOM_SNAPSHOT_HTML_BYTES + 1))),
            ("snapshot", json!("")),
            ("element_count", json!(-1)),
            ("excluded", json!({"iframes": 0})),
        ] {
            let mut b = base.clone();
            b[k] = v;
            assert!(parse(&b).is_err(), "{k}");
        }
    }
}
