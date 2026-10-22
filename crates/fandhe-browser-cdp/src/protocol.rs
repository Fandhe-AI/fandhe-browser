//! CDP の JSON-RPC メッセージ型とメソッドディスパッチャ
//! （TASK-42（42.1）・#239、ビヘイビア `CDP-1`・`CDP-5`・`CDP-6`・`SEC-2`・MS-4）。
//!
//! `crate::ws::BrowserSessionHandler` が受信した WebSocket テキストフレームを
//! [`Dispatcher::dispatch`] へ渡し、返ってきた [`DispatchOutcome`]（レスポンス 1 件と
//! イベント列）を JSON 文字列へ直列化して送り返す。本モジュールはトランスポート
//! （WebSocket・HTTP）の型を一切参照せず、フレームとの変換は ws.rs の責務とする。
//!
//! 公開入口は無く crate 内部専用。後続タスクは [`builtin_handlers`] へメソッドを
//! 追加するだけでよい（`Page.navigate` は 42.2 で登録済み、イベント送出は 42.3 で実装済み、
//! `DOM.getDocument`・`DOM.querySelector` は 42.4・42.5、`DOM.requestChildNodes` は #657、`Browser.getVersion` は TASK-43.3a、`Target.getTargetInfo` は TASK-43.3b、`Target.setAutoAttach`・`Browser.setDownloadBehavior` は TASK-43.3c、`Target.createBrowserContext` は TASK-43.3d、`Target.closeTarget` は TASK-43.3h、`Target.createTarget`・`Runtime.runIfWaitingForDebugger` は TASK-43.3e、`Page.enable`・`Page.getFrameTree` は TASK-43.3i、`Log.enable`・`Runtime.enable`・`Network.enable` は TASK-43.3j、`Page.setLifecycleEventsEnabled` は TASK-43.3k、`Emulation.setFocusEmulationEnabled`・`Emulation.setEmulatedMedia` は TASK-43.3m、`Page.createIsolatedWorld` は TASK-44.6（#1217）で登録済み）。
//!
//! # 入力の扱い
//!
//! 受信メッセージは untrusted。`serde` の derive は使わず `serde_json::Value` を
//! `get()` / `as_*()` で明示的に検査し（添字・`unwrap` 不使用）、応答へ反映してよいのは
//! 整数として検証済みの `id` と [`SessionId::parse`] 検証済みの `sessionId` のみ。
//! エラー文言は固定文言（[`CdpError`]）で method 名などの入力値をエコーしない。
//! メッセージ全体のサイズ上限は core の WebSocket 既定値に任せ、ここでは再実装しない。
//!
//! # 未実装メソッドの応答方針（`CDP-6`・`SEC-2`・TASK-42.6・#244）
//!
//! [`builtin_handlers`] には `Page.navigate`（TASK-42.2）・`DOM.getDocument`（TASK-42.4）・
//! `DOM.querySelector`（TASK-42.5）・`DOM.requestChildNodes`（#657）・
//! `Browser.getVersion`（TASK-43.3a・`CDP-2`）・`Target.getTargetInfo`（TASK-43.3b・`CDP-2`）・`Target.setAutoAttach`・`Browser.setDownloadBehavior`（TASK-43.3c・`CDP-2`）・`Target.createBrowserContext`（TASK-43.3d・`CDP-2`）・`Target.closeTarget`（TASK-43.3h・`CDP-2`）・`Target.createTarget`・`Runtime.runIfWaitingForDebugger`（TASK-43.3e・#910・`CDP-2`）・`Page.enable`・`Page.getFrameTree`（TASK-43.3i・#1037・`CDP-2`）・`Log.enable`・`Runtime.enable`・`Network.enable`（TASK-43.3j・#1038・`CDP-2`）・`Page.setLifecycleEventsEnabled`（TASK-43.3k・#1039・`CDP-2`）・`Emulation.setFocusEmulationEnabled`・`Emulation.setEmulatedMedia`（TASK-43.3m・#1041・`CDP-2`）・`Page.createIsolatedWorld`（TASK-44.6・#1217・`CDP-2`）・`Runtime.evaluate`・`Runtime.releaseObject`（TASK-44.7・#1218・`CDP-2`）のみ登録済みで、
//! それ以外のメソッドは JSON-RPC の「method not implemented」（`-32601`）エラーを返す。
//! 汎用の空 success フォールバック（PoC-5 の暫定挙動）は採らない。理由は次のとおり。
//!
//! - `SEC-2`・security.md「偽装・回避機能の禁止」: 何にでも `{}` を成功で返すと、
//!   `Emulation.setUserAgentOverride`・`Network.setUserAgentOverride`・
//!   `Page.addScriptToEvaluateOnNewDocument` など stealth 系が使う呼び出しが、効いていないのに
//!   成功したように見える。これは検出回避・偽装と同じ形の挙動になるため採らない（レビュー確認済み）
//! - `REPAIR-3`: 実装済みを装わない。呼び出し側が未対応を検知して迂回・報告できるようにする
//! - PoC-5 のフォールバックは必須メソッドを実測で洗い出す一時手段だった。その目的は
//!   下記の受信ログが引き継ぐため、フォールバックは不要
//! - Chromium は `'X' wasn't found` とメソッド名を返すが、本 crate は 42.1 の方針（固定文言・
//!   入力値を応答へ反映しない）を守る意図的な差異とする
//! - 今後、何もしないと分かっているメソッドへ成功を返す必要が出たら、`builtin_handlers` へ
//!   メソッド単位で個別登録し `SEC-2` の観点でレビューする。汎用フォールバック・前方一致での
//!   一括成功は追加しない。Playwright 互換（`CDP-2`）のための no-op の要否は TASK-43 で判断する
//!
//! # 受信ログ（`CDP-6`）
//!
//! パースと名前検証を通ったリクエストのメソッド名と件数を `CdpState` の受信ログ
//! （[`CdpState::received_methods`]）へ記録する（`id`・`sessionId`・`params` は記録しない。
//! 未実装名の保持数に上限あり。`crate::method_log`）。未実装メソッドは名前の初出時に 1 回だけ
//! stderr へ 1 行出す（保持上限到達後も一定件数までは出す）。本番の出力先は #221（TASK-10.3）で決める。

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use fandhe_browser_core::FetchOptions;
use serde_json::{Map, Value, json};

use crate::method_log::{MethodDisposition, RecordResult};
use crate::page::PageNavigate;
use crate::server::CdpState;
use crate::target::SessionId;

/// メソッド名の最大バイト数（外部入力・登録名の双方に適用）。
pub(crate) const MAX_METHOD_LEN: usize = 128;

/// 非同期ハンドラの戻り値型（std のみで定義し、トランスポート crate に依存しない）。
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// JSON-RPC 2.0 / CDP のエラーコードを持つ固定文言エラー。
///
/// 入力値を含められない型にして、応答経由のインジェクション・情報反射を構造的に防ぐ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CdpError {
    /// JSON-RPC エラーコード。
    pub code: i64,
    /// 固定の英語メッセージ。
    pub message: &'static str,
}

// 後続タスク（42.2 以降）のハンドラが使う定数を含むため dead_code を許容する。
#[allow(dead_code)]
impl CdpError {
    /// JSON として解釈できない。
    pub const PARSE_ERROR: Self = Self::new(-32700, "parse error");
    /// リクエストの形が不正。
    pub const INVALID_REQUEST: Self = Self::new(-32600, "invalid request");
    /// メソッドが登録されていない。成功を返さず固定文言のエラーにする方針（`CDP-6`・`SEC-2`。モジュールドキュメント参照）。
    pub const METHOD_NOT_FOUND: Self = Self::new(-32601, "method not implemented");
    /// `params` が不正。
    pub const INVALID_PARAMS: Self = Self::new(-32602, "invalid params");
    /// ハンドラ内部エラー（CDP の server error 帯）。
    pub const SERVER_ERROR: Self = Self::new(-32000, "server error");
    /// WebSocket 接続を要するコマンドが接続なし（直接 dispatch）で呼ばれた、または接続が既に閉じている
    /// （`Target.setAutoAttach`・`Browser.setDownloadBehavior`。接続スコープの状態を持てない。`CDP-2`）。
    pub const NO_CONNECTION: Self = Self::new(-32000, "no connection");
    /// 直近の navigate 結果が無い（未 navigate・取得中・取得失敗を区別しない。`DOM.getDocument`）。
    pub const NO_DOCUMENT: Self = Self::new(-32000, "no document loaded");
    /// 未実装のオプション（`DOM.getDocument` の `pierce: true` 等）。成功を装わず明示的に拒否する（`REPAIR-3`）。
    pub const UNSUPPORTED_PARAMS: Self = Self::new(-32602, "unsupported params");
    /// 文書のノード数が採番の処理量上限を超えた（`DOM.getDocument`。`SEC-2`）。
    pub const DOCUMENT_TOO_LARGE: Self = Self::new(-32000, "document too large");
    /// そのターゲットに確定済みの文書が無い（未遷移・遷移中・取得失敗。`DOM.getDocument` の `sessionId` 付き。
    /// 空文書を捏造せず明示エラーにする。`CDP-1`・`REPAIR-3`・`SEC-2`）。
    pub const TARGET_DOCUMENT_UNAVAILABLE: Self =
        Self::new(-32000, "document not available for target");
    /// `waitForDebuggerOnStart` で止めたターゲットが再開前に `Page.navigate` を受けた（再開時の初期読み込みと
    /// 競合して遷移を中断するのを防ぐ。`Runtime.runIfWaitingForDebugger` で再開する。#910・`CDP-2`）。
    pub const TARGET_WAITING_FOR_DEBUGGER: Self =
        Self::new(-32000, "target is waiting for debugger");
    /// セッションあたりの init script 登録数が上限に達した（`Page.addScriptToEvaluateOnNewDocument`。`CDP-7`）。
    pub const INIT_SCRIPT_LIMIT_EXCEEDED: Self = Self::new(-32000, "too many init scripts");
    /// `worldName` が長すぎる（`Page.addScriptToEvaluateOnNewDocument`。`CDP-7`）。
    pub const WORLD_NAME_TOO_LONG: Self = Self::new(-32000, "world name too long");
    /// 評価器（core の `ScriptContextHandle`）が無効で、実行コンテキストを作れない（`Page.createIsolatedWorld`。
    /// 使えない contextId を渡さず明示エラーにする。`REPAIR-3`・`SEC-2`・TASK-44.6）。
    pub const EXECUTION_CONTEXTS_UNAVAILABLE: Self =
        Self::new(-32000, "execution contexts unavailable");
    /// 異なる `worldName` の数、または文書 × セッションあたりの isolated world 数が上限に達した
    /// （`Page.addScriptToEvaluateOnNewDocument`・`Page.createIsolatedWorld`。`CDP-7`）。
    pub const TOO_MANY_WORLDS: Self = Self::new(-32000, "too many worlds");
    /// 古い世代（現在の文書でない）の contextId が評価に使われた（評価系 #1218 が使う。`CDP-2`）。
    pub const EXECUTION_CONTEXT_STALE: Self = Self::new(-32000, "execution context is not current");
    /// `Runtime.evaluate` の `expression`（または評価用に包んだソース）が上限を超えた（`CDP-7`・`SEC-2`。
    /// 評価器を起動しない。TASK-44.7・#1218）。
    pub const EXPRESSION_TOO_LARGE: Self = Self::new(-32000, "expression too large");
    /// main world（既定コンテキスト）への評価は未対応（`Runtime.evaluate`。成功を装わず固定文言で拒否する。
    /// `REPAIR-3`・TASK-44.7・#1218）。
    pub const MAIN_WORLD_EVALUATION_UNSUPPORTED: Self =
        Self::new(-32000, "evaluation in the main world is not supported");
    /// 評価が時間切れになった（JS の例外ではない。`CDP-7`・#1218）。
    pub const EVALUATION_TIMED_OUT: Self = Self::new(-32000, "evaluation timed out");
    /// 評価が資源上限（ヒープ・コンテキスト数・待ち行列）に達した（JS の例外ではない。`CDP-7`・#1218）。
    pub const EVALUATION_RESOURCE_LIMIT: Self =
        Self::new(-32000, "evaluation resource limit exceeded");
    /// 評価結果が大きすぎる（`CDP-7`・#1218）。
    pub const EVALUATION_RESULT_TOO_LARGE: Self = Self::new(-32000, "evaluation result too large");
    /// コンテキスト内のリモートオブジェクト（ハンドル）数が上限に達した（`CDP-7`・#1218）。
    pub const TOO_MANY_REMOTE_OBJECTS: Self = Self::new(-32000, "too many remote objects");
    /// `objectId` が指す値が無い（未知・他セッション・解放済みを区別しない。`Runtime.releaseObject`・#1218）。
    pub const REMOTE_OBJECT_NOT_FOUND: Self = Self::new(-32000, "remote object not found");
    /// `awaitPromise` で microtask checkpoint の後も Promise が未決着（成功を装わない。`CDP-6`・`REPAIR-3`・#1219）。
    pub const PROMISE_NOT_SETTLED: Self = Self::new(-32000, "promise was not settled");
    /// `Runtime.callFunctionOn` の `arguments` が上限（64 件）を超えた（`CDP-7`・`SEC-2`・#1219）。
    pub const TOO_MANY_CALL_ARGUMENTS: Self = Self::new(-32000, "too many call arguments");
    /// `Runtime.callFunctionOn` の引数の `objectId` が `this` と別の実行コンテキストのもの（#1219）。
    pub const ARGUMENT_CONTEXT_MISMATCH: Self =
        Self::new(-32000, "argument belongs to another execution context");
    /// `Runtime.callFunctionOn` の `functionDeclaration` が関数に評価されなかった（#1219）。
    pub const NOT_A_FUNCTION: Self = Self::new(
        -32000,
        "function declaration did not evaluate to a function",
    );
    /// `returnByValue` で値にできない（循環参照・関数・symbol 等。`REPAIR-3`・#1218）。
    pub const VALUE_NOT_SERIALIZABLE: Self =
        Self::new(-32000, "value could not be returned by value");
    /// 評価コンテキストが作り直された（タイムアウト後の再起動等）ため、contextId を失効させた（#1218）。
    pub const EXECUTION_CONTEXT_RESET: Self = Self::new(-32000, "execution context was reset");
    /// 指定 nodeId が現在の文書の採番表に無い（形式は正しいが対象が無い。`DOM.querySelector`。`CDP-1`）。
    pub const NODE_NOT_FOUND: Self = Self::new(-32000, "node not found");
    /// nodeId の払い出し記録が件数上限に達した（`DOM.getDocument`。他クライアントの記録を
    /// 追い出さず新規の払い出しを拒否する。`SEC-2`）。
    pub const ISSUED_LIMIT_EXCEEDED: Self = Self::new(-32000, "too many issued documents");

    const fn new(code: i64, message: &'static str) -> Self {
        Self { code, message }
    }
}

/// CDP コマンド（クライアント → サーバー）。`parse_request` が検証済みの値だけを持つ。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CdpRequest {
    /// クライアントが採番した整数 ID（応答へエコーする）。
    pub id: i64,
    /// `Domain.method` 形式のメソッド名（長さ・文字種検証済み）。
    pub method: String,
    /// 必ず JSON オブジェクト（省略時は `{}`）。
    pub params: Value,
    /// flatten モードのセッション ID（検証済み）。
    pub session_id: Option<SessionId>,
}

/// レスポンス本体。成功かエラーのどちらか一方。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ResponseBody {
    /// `result`（必ず JSON オブジェクト）。
    Result(Value),
    /// `error`。
    Error(CdpError),
}

/// CDP レスポンス（サーバー → クライアント）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CdpResponse {
    /// 対応するコマンドの ID。ID を特定できない不正リクエストでは `None`。
    pub id: Option<i64>,
    /// リクエストの `sessionId` のエコー。
    pub session_id: Option<SessionId>,
    /// 成功またはエラー。
    pub body: ResponseBody,
}

impl CdpResponse {
    /// `{"id":N,"result":{..}}` / `{"id":N,"error":{"code":C,"message":".."}}` へ直列化する。
    /// `id`・`sessionId` は存在するときだけキーを出す。
    pub fn to_json(&self) -> String {
        let mut m = Map::new();
        if let Some(id) = self.id {
            m.insert("id".into(), json!(id));
        }
        match &self.body {
            ResponseBody::Result(v) => {
                m.insert("result".into(), v.clone());
            }
            ResponseBody::Error(e) => {
                m.insert(
                    "error".into(),
                    json!({"code": e.code, "message": e.message}),
                );
            }
        }
        if let Some(s) = &self.session_id {
            m.insert("sessionId".into(), json!(s.as_str()));
        }
        Value::Object(m).to_string()
    }
}

/// CDP イベント（サーバー → クライアント。`id` を持たない通知）。
///
/// `Page.navigate` が確定時に `Page.frameNavigated`・`Page.loadEventFired` を送出する（TASK-42.3）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CdpEvent {
    /// イベント名。サーバー側の定数に限る。
    pub method: &'static str,
    /// イベントパラメータ（JSON オブジェクト）。
    pub params: Value,
    /// 送出先セッション。
    pub session_id: Option<SessionId>,
}

impl CdpEvent {
    /// `{"method":"..","params":{..}}`（`sessionId` があれば付与）へ直列化する。
    pub fn to_json(&self) -> String {
        let mut m = Map::new();
        m.insert("method".into(), json!(self.method));
        m.insert("params".into(), self.params.clone());
        if let Some(s) = &self.session_id {
            m.insert("sessionId".into(), json!(s.as_str()));
        }
        Value::Object(m).to_string()
    }
}

/// ハンドラの成功時出力。戻り値を構造体にして、イベント同伴などの拡張で
/// シグネチャを変えずに済ませる（`REPAIR-4`）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HandlerOutput {
    /// `result` に入れる JSON オブジェクト。
    pub result: Value,
    /// 応答の**後**に送出するイベント（順序どおり。既定）。
    pub events: Vec<CdpEvent>,
    /// 応答の**前**に送出するイベント（順序どおり。opt-in）。`Target.createTarget` の
    /// `Target.attachedToTarget` 用で、Playwright は応答を受けた時点で当該ターゲットのページが
    /// 存在する前提で動くため（`CDP-5`・`CDP-2`・#910）。他のハンドラは使わない。
    pub pre_events: Vec<CdpEvent>,
}

impl HandlerOutput {
    /// イベントなしの出力。`result` は JSON オブジェクトを渡す。
    pub fn result(result: Value) -> Self {
        Self {
            result,
            events: Vec::new(),
            pre_events: Vec::new(),
        }
    }

    /// 応答より前に送出するイベントを末尾へ追加する（`CDP-5`。使い所は `Target.createTarget` のみ）。
    pub fn with_pre_event(mut self, event: CdpEvent) -> Self {
        self.pre_events.push(event);
        self
    }

    /// 同伴イベントを末尾へ追加する。
    pub fn with_event(mut self, event: CdpEvent) -> Self {
        self.events.push(event);
        self
    }
}

/// [`Dispatcher::dispatch`] の結果。送出順は「応答前イベント → レスポンス → イベント列」
/// （既定は応答前イベントなしの「レスポンス → イベント列」。応答前に送るのは
/// `Target.createTarget` の attach 通知だけ。`CDP-5`・`CDP-2`・#910）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DispatchOutcome {
    /// 応答の前に送るイベント（[`HandlerOutput::pre_events`]）。
    pub pre_events: Vec<CdpEvent>,
    /// コマンドへの応答。
    pub response: CdpResponse,
    /// 応答の後に送るイベント。
    pub events: Vec<CdpEvent>,
}

impl DispatchOutcome {
    /// 送出すべき JSON 文字列を順序どおり返す（レスポンス、イベント列）。
    pub fn into_frames(self) -> Vec<String> {
        let mut frames = Vec::with_capacity(1 + self.pre_events.len() + self.events.len());
        frames.extend(self.pre_events.iter().map(CdpEvent::to_json));
        frames.push(self.response.to_json());
        frames.extend(self.events.iter().map(CdpEvent::to_json));
        frames
    }
}

/// パース失敗。エラー応答へそのまま変換できる形で保持する。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParseFailure {
    /// 整数 ID が確定していれば `Some`。
    pub id: Option<i64>,
    /// 失敗内容。
    pub error: CdpError,
}

fn method_name_is_valid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_METHOD_LEN
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

/// 受信テキストを [`CdpRequest`] へ検証つきで変換する。
///
/// 判定順: JSON 不正 → `-32700`、オブジェクトでない / `id` が整数でない → `-32600`（ID なし）、
/// `method`・`sessionId` 不正 → `-32600`（ID 付き）、`params` がオブジェクトでない → `-32602`（ID 付き）。
pub(crate) fn parse_request(text: &str) -> Result<CdpRequest, ParseFailure> {
    let fail = |id: Option<i64>, error: CdpError| ParseFailure { id, error };
    let value: Value = serde_json::from_str(text).map_err(|_| fail(None, CdpError::PARSE_ERROR))?;
    let obj = value
        .as_object()
        .ok_or_else(|| fail(None, CdpError::INVALID_REQUEST))?;
    let id = obj
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| fail(None, CdpError::INVALID_REQUEST))?;
    let bad = |e: CdpError| fail(Some(id), e);

    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .filter(|m| method_name_is_valid(m))
        .ok_or_else(|| bad(CdpError::INVALID_REQUEST))?
        .to_owned();
    let params = match obj.get("params") {
        None => Value::Object(Map::new()),
        Some(p) if p.is_object() => p.clone(),
        Some(_) => return Err(bad(CdpError::INVALID_PARAMS)),
    };
    let session_id = match obj.get("sessionId") {
        None => None,
        Some(s) => Some(
            s.as_str()
                .and_then(|s| SessionId::parse(s).ok())
                .ok_or_else(|| bad(CdpError::INVALID_REQUEST))?,
        ),
    };
    Ok(CdpRequest {
        id,
        method,
        params,
        session_id,
    })
}

/// WebSocket 接続 1 本を表す crate 内の識別子（`crate::ws` が接続ごとに割り当てる。`CDP-1`）。
///
/// ブラウザレベルの状態（`DOM.getDocument` の nodeId 払い出し記録等）を接続単位で分離するための
/// キー。プロセス内連番で推測可能なため認可には使わない（識別子であって資格情報ではない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ConnId(u64);

impl ConnId {
    /// 連番から構築する。
    pub(crate) fn new(n: u64) -> Self {
        Self(n)
    }
}

/// ハンドラが参照する実行コンテキスト。
// フィールドは 42.2 以降のハンドラが読む（骨格のみ先行）ため dead_code を許容する。
#[allow(dead_code)]
pub(crate) struct CommandContext<'a> {
    /// CDP 状態（`app_state()` 経由で core の `AppState` を更新できる。42.2 で使用）。
    pub state: &'a Arc<CdpState>,
    /// リクエストの `sessionId`。
    pub session_id: Option<&'a SessionId>,
    /// 要求元の WebSocket 接続。WebSocket を介さない直接 dispatch（テスト）では `None`。
    pub conn: Option<ConnId>,
}

/// 1 メソッド分のハンドラ。
///
/// 失敗は `Err(CdpError)` で返す契約。release は `panic = "abort"` 前提のため
/// panic を捕捉せず、ハンドラは panic してはならない。
pub(crate) trait CommandHandler: Send + Sync + 'static {
    /// `params`（必ず JSON オブジェクト）を処理して出力を返す。
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>>;
}

/// ディスパッチャ構築時のエラー。文言は固定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatcherError {
    /// 登録名が空・長すぎる・使用不可文字を含む。
    InvalidMethodName,
    /// 同名メソッドの重複登録（黙って上書きしない）。
    DuplicateMethod,
    /// 組込みハンドラの初期化に失敗した（例: HTTP クライアント構築失敗）。
    HandlerInit,
}

impl fmt::Display for DispatcherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidMethodName => "invalid command method name",
            Self::DuplicateMethod => "duplicate command method",
            Self::HandlerInit => "command handler initialization failed",
        })
    }
}

impl std::error::Error for DispatcherError {}

/// 登録済みハンドラ表。
type HandlerTable = Vec<(&'static str, Box<dyn CommandHandler>)>;

/// 組込みハンドラ表。`Page.navigate`（TASK-42.2）・`DOM.getDocument`（TASK-42.4）・
/// `DOM.querySelector`（TASK-42.5）・`DOM.requestChildNodes`（#657）・`Browser.getVersion`（TASK-43.3a）・
/// `Target.getTargetInfo`（TASK-43.3b）・`Target.setAutoAttach`・`Browser.setDownloadBehavior`（TASK-43.3c。実体に接続済み）・`Target.createBrowserContext`（TASK-43.3d）・`Target.closeTarget`（TASK-43.3h）・`Target.createTarget`・`Runtime.runIfWaitingForDebugger`（TASK-43.3e）・`Page.enable`・`Page.getFrameTree`（TASK-43.3i・#1037）・`Log.enable`・`Runtime.enable`・`Network.enable`（TASK-43.3j・#1038）・`Page.setLifecycleEventsEnabled`（TASK-43.3k・#1039）・`Page.addScriptToEvaluateOnNewDocument`（空 source のみ。TASK-43.3l・#1040）・`Emulation.setFocusEmulationEnabled`・`Emulation.setEmulatedMedia`（TASK-43.3m・#1041）・`Page.createIsolatedWorld`（TASK-44.6・#1217）・`Runtime.evaluate`・`Runtime.releaseObject`（TASK-44.7・#1218。isolated world のみ）を登録済み（`CDP-1`・`CDP-2`）。
///
/// `Page.navigate` は既定の `FetchOptions`（内部アドレス拒否）で `Fetcher` を構築する。
pub(crate) fn builtin_handlers() -> Result<HandlerTable, DispatcherError> {
    builtin_handlers_with(FetchOptions::default())
}

/// [`builtin_handlers`] の `Page.navigate` が使う `FetchOptions` を差し替えられる版。
/// 本番経路は常に [`builtin_handlers`]（内部アドレス拒否）を使い、緩めた設定を渡すのは
/// feature `test-support` の結合テスト入口だけ（`CDP-2`・TASK-44.3・SSRF 防御）。
pub(crate) fn builtin_handlers_with(fetch: FetchOptions) -> Result<HandlerTable, DispatcherError> {
    let navigate = PageNavigate::new(fetch).map_err(|_| DispatcherError::HandlerInit)?;
    Ok(vec![
        ("Page.navigate", Box::new(navigate)),
        (
            "Browser.getVersion",
            Box::new(crate::playwright_compat::BrowserGetVersion),
        ),
        (
            "Target.getTargetInfo",
            Box::new(crate::playwright_compat::TargetGetTargetInfo),
        ),
        (
            "Target.setAutoAttach",
            Box::new(crate::playwright_compat::TargetSetAutoAttach),
        ),
        (
            "Browser.setDownloadBehavior",
            Box::new(crate::playwright_compat::BrowserSetDownloadBehavior),
        ),
        (
            "Target.createBrowserContext",
            Box::new(crate::playwright_compat::TargetCreateBrowserContext),
        ),
        (
            "Target.closeTarget",
            Box::new(crate::playwright_compat::TargetCloseTarget),
        ),
        (
            "Target.createTarget",
            Box::new(crate::playwright_compat::TargetCreateTarget),
        ),
        (
            "Runtime.runIfWaitingForDebugger",
            Box::new(crate::playwright_compat::RuntimeRunIfWaitingForDebugger),
        ),
        (
            "Page.enable",
            Box::new(crate::playwright_compat::PageEnable),
        ),
        (
            "Page.addScriptToEvaluateOnNewDocument",
            Box::new(crate::playwright_compat::PageAddScriptToEvaluateOnNewDocument),
        ),
        (
            "Page.setLifecycleEventsEnabled",
            Box::new(crate::playwright_compat::PageSetLifecycleEventsEnabled),
        ),
        (
            "Emulation.setFocusEmulationEnabled",
            Box::new(crate::playwright_compat::EmulationSetFocusEmulationEnabled),
        ),
        (
            "Emulation.setEmulatedMedia",
            Box::new(crate::playwright_compat::EmulationSetEmulatedMedia),
        ),
        (
            "Page.createIsolatedWorld",
            Box::new(crate::runtime::PageCreateIsolatedWorld),
        ),
        (
            "Runtime.evaluate",
            Box::new(crate::runtime::RuntimeEvaluate::new(
                crate::runtime::EvalLimits::default(),
            )),
        ),
        (
            "Runtime.releaseObject",
            Box::new(crate::runtime::RuntimeReleaseObject),
        ),
        (
            "Runtime.callFunctionOn",
            Box::new(crate::runtime::RuntimeCallFunctionOn::new(
                crate::runtime::EvalLimits::default(),
            )),
        ),
        ("Log.enable", Box::new(crate::playwright_compat::LogEnable)),
        (
            "Runtime.enable",
            Box::new(crate::playwright_compat::RuntimeEnable),
        ),
        (
            "Network.enable",
            Box::new(crate::playwright_compat::NetworkEnable),
        ),
        (
            "Page.getFrameTree",
            Box::new(crate::playwright_compat::PageGetFrameTree),
        ),
        ("DOM.getDocument", Box::new(crate::dom::DomGetDocument)),
        ("DOM.querySelector", Box::new(crate::dom::DomQuerySelector)),
        (
            "DOM.requestChildNodes",
            Box::new(crate::dom::DomRequestChildNodes),
        ),
    ])
}

/// メソッド名からハンドラへ振り分けるディスパッチャ。構築後は不変。
pub(crate) struct Dispatcher {
    handlers: HashMap<&'static str, Box<dyn CommandHandler>>,
}

impl Dispatcher {
    /// 組込みハンドラ表（[`builtin_handlers`]）から構築する。ws.rs の接続設定から呼ばれる。
    pub fn builtin() -> Result<Self, DispatcherError> {
        Self::from_handlers(builtin_handlers()?)
    }

    /// 組込みハンドラ表を `Page.navigate` の `FetchOptions` 差し替え付きで構築する
    /// （feature `test-support` の結合テスト入口専用。`CDP-2`・TASK-44.3）。
    #[cfg(feature = "test-support")]
    pub fn builtin_with(fetch: FetchOptions) -> Result<Self, DispatcherError> {
        Self::from_handlers(builtin_handlers_with(fetch)?)
    }

    /// 任意のハンドラ表から構築する。名前の検証と重複検出を行う。
    pub fn from_handlers(handlers: HandlerTable) -> Result<Self, DispatcherError> {
        let mut map: HashMap<&'static str, Box<dyn CommandHandler>> = HashMap::new();
        for (name, h) in handlers {
            if !method_name_is_valid(name) {
                return Err(DispatcherError::InvalidMethodName);
            }
            if map.insert(name, h).is_some() {
                return Err(DispatcherError::DuplicateMethod);
            }
        }
        Ok(Self { handlers: map })
    }

    /// 受信テキスト 1 件を処理する。パース失敗・未登録メソッド・ハンドラのエラーは
    /// すべてエラー応答になり、成功応答を捏造しない（`SEC-2`）。パースと名前検証を通った
    /// メソッド名だけが受信ログへ記録される（パース失敗は記録しない。`CDP-6`）。
    #[cfg(test)]
    pub async fn dispatch(&self, state: &Arc<CdpState>, text: &str) -> DispatchOutcome {
        self.dispatch_on(state, None, text).await
    }

    /// [`Self::dispatch`] の接続指定版。ws.rs の `on_message_with_ctx` が接続 ID 付きで呼ぶ。
    pub async fn dispatch_on(
        &self,
        state: &Arc<CdpState>,
        conn: Option<ConnId>,
        text: &str,
    ) -> DispatchOutcome {
        let req = match parse_request(text) {
            Ok(r) => r,
            Err(f) => {
                let events = auto_attach_events(state, conn, true);
                return outcome(f.id, None, ResponseBody::Error(f.error), events);
            }
        };
        let handler = self.handlers.get(req.method.as_str());
        let kind = if handler.is_some() {
            MethodDisposition::Handled
        } else {
            MethodDisposition::Unimplemented
        };
        let recorded = state.method_log().record(&req.method, kind);
        // 保持上限到達後も出力枠内（DroppedReport）は名前を出し、必須メソッドの洗い出しを妨げない。
        if kind == MethodDisposition::Unimplemented
            && matches!(
                recorded,
                RecordResult::FirstSeen | RecordResult::DroppedReport
            )
        {
            // 名前は method_name_is_valid 済み（ASCII 英数字と `.`）なのでログ注入の恐れはない。
            eprintln!("cdp: unimplemented method received: {}", req.method);
        }
        let Some(handler) = handler else {
            return outcome(
                Some(req.id),
                req.session_id,
                ResponseBody::Error(CdpError::METHOD_NOT_FOUND),
                auto_attach_events(state, conn, true),
            );
        };
        let ctx = CommandContext {
            state,
            session_id: req.session_id.as_ref(),
            conn,
        };
        match handler.handle(ctx, &req.params).await {
            Ok(out) => {
                let fill = |mut e: CdpEvent| {
                    if e.session_id.is_none() {
                        e.session_id = req.session_id.clone();
                    }
                    e
                };
                let pre_events: Vec<CdpEvent> = out.pre_events.into_iter().map(fill).collect();
                let mut events: Vec<CdpEvent> = out.events.into_iter().map(fill).collect();
                events.extend(auto_attach_events(state, conn, true));
                let mut o = outcome(
                    Some(req.id),
                    req.session_id.clone(),
                    ResponseBody::Result(out.result),
                    events,
                );
                o.pre_events = pre_events;
                o
            }
            Err(e) => outcome(
                Some(req.id),
                req.session_id,
                ResponseBody::Error(e),
                // 失敗した要求（原子的 attach の巻き戻し直後等）では新規 attach を再試行しない。
                auto_attach_events(state, conn, false),
            ),
        }
    }
}

/// 自動 attach 有効中の接続について、新ターゲットの attach と消失セッションの回収を行い、
/// ブラウザレベルのイベントとして返す（push 経路が無いため次のコマンド応答に同伴する。
/// 成功・失敗・未実装メソッドの全経路から呼び、取りこぼしを防ぐ（ハンドラ失敗時は `attach_new=false` で回収のみ）。`CDP-2`・`CDP-7`）。
/// セッション枠の枯渇は応答を失敗にせず stderr へ記録し、次回に再試行する。
fn auto_attach_events(
    state: &Arc<CdpState>,
    conn: Option<ConnId>,
    attach_new: bool,
) -> Vec<CdpEvent> {
    // 止めたセッションが外れたターゲットを再開する（永久停止を残さない。`CDP-7`・#910）。
    state.resume_orphaned_paused();
    let Some(c) = conn else {
        return Vec::new();
    };
    let rec = state
        .conn_scoped()
        .reconcile_auto_attach(c, state.registry(), attach_new);
    if rec.error.is_some() {
        eprintln!("cdp: auto-attach could not attach every target (session limit); will retry");
    }
    let mut events = Vec::new();
    for (target_id, sid) in &rec.detached {
        events.push(crate::playwright_compat::detached_event(target_id, sid));
    }
    for (info, sid) in &rec.attached {
        events.push(crate::playwright_compat::attached_event(info, sid, false));
    }
    events
}

fn outcome(
    id: Option<i64>,
    session_id: Option<SessionId>,
    body: ResponseBody,
    events: Vec<CdpEvent>,
) -> DispatchOutcome {
    DispatchOutcome {
        pre_events: Vec::new(),
        response: CdpResponse {
            id,
            session_id,
            body,
        },
        events,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_err(text: &str) -> (Option<i64>, i64) {
        let f = parse_request(text).unwrap_err();
        (f.id, f.error.code)
    }

    #[test]
    fn cdp1_parse_accepts_minimal_and_full_request() {
        let r = parse_request(r#"{"id":1,"method":"Page.navigate"}"#).unwrap();
        assert_eq!(r.id, 1);
        assert_eq!(r.method, "Page.navigate");
        assert_eq!(r.params, json!({}));
        assert_eq!(r.session_id, None);

        let r = parse_request(
            r#"{"id":2,"method":"DOM.getDocument","params":{"depth":1},"sessionId":"AB-12"}"#,
        )
        .unwrap();
        assert_eq!(r.params, json!({"depth": 1}));
        assert_eq!(r.session_id.unwrap().as_str(), "AB-12");
    }

    #[test]
    fn cdp1_parse_failures_use_expected_codes_and_ids() {
        let too_long = format!(
            r#"{{"id":3,"method":"{}"}}"#,
            "A".repeat(MAX_METHOD_LEN + 1)
        );
        assert_eq!(parse_err("not json"), (None, -32700));
        assert_eq!(parse_err("[1,2]"), (None, -32600));
        assert_eq!(parse_err(r#"{"method":"x"}"#), (None, -32600));
        assert_eq!(parse_err(r#"{"id":"1","method":"x"}"#), (None, -32600));
        assert_eq!(parse_err(r#"{"id":1.5,"method":"x"}"#), (None, -32600));
        assert_eq!(
            parse_err(r#"{"id":18446744073709551615,"method":"x"}"#),
            (None, -32600)
        );
        assert_eq!(parse_err(r#"{"id":3}"#), (Some(3), -32600));
        assert_eq!(parse_err(r#"{"id":3,"method":5}"#), (Some(3), -32600));
        assert_eq!(parse_err(r#"{"id":3,"method":""}"#), (Some(3), -32600));
        assert_eq!(parse_err(r#"{"id":3,"method":"A b"}"#), (Some(3), -32600));
        assert_eq!(parse_err(&too_long), (Some(3), -32600));
        assert_eq!(
            parse_err(r#"{"id":4,"method":"A.b","params":[1]}"#),
            (Some(4), -32602)
        );
        assert_eq!(
            parse_err(r#"{"id":5,"method":"A.b","sessionId":"bad id!"}"#),
            (Some(5), -32600)
        );
        assert_eq!(
            parse_err(r#"{"id":5,"method":"A.b","sessionId":7}"#),
            (Some(5), -32600)
        );
    }

    #[test]
    fn cdp1_response_and_event_json_shapes() {
        let ok = CdpResponse {
            id: Some(1),
            session_id: Some(SessionId::parse("S1").unwrap()),
            body: ResponseBody::Result(json!({"a": 1})),
        };
        let v: Value = serde_json::from_str(&ok.to_json()).unwrap();
        assert_eq!(v, json!({"id": 1, "result": {"a": 1}, "sessionId": "S1"}));

        let err = CdpResponse {
            id: None,
            session_id: None,
            body: ResponseBody::Error(CdpError::PARSE_ERROR),
        };
        let v: Value = serde_json::from_str(&err.to_json()).unwrap();
        assert_eq!(
            v,
            json!({"error": {"code": -32700, "message": "parse error"}})
        );

        let ev = CdpEvent {
            method: "Page.loadEventFired",
            params: json!({"timestamp": 1}),
            session_id: None,
        };
        let v: Value = serde_json::from_str(&ev.to_json()).unwrap();
        assert_eq!(
            v,
            json!({"method": "Page.loadEventFired", "params": {"timestamp": 1}})
        );
    }

    struct Nop;
    impl CommandHandler for Nop {
        fn handle<'a>(
            &'a self,
            _: CommandContext<'a>,
            _: &'a Value,
        ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
            Box::pin(async { Ok(HandlerOutput::result(json!({}))) })
        }
    }

    #[test]
    fn cdp1_dispatcher_rejects_duplicate_and_invalid_names() {
        let dup: HandlerTable = vec![("A.b", Box::new(Nop)), ("A.b", Box::new(Nop))];
        assert_eq!(
            Dispatcher::from_handlers(dup).err(),
            Some(DispatcherError::DuplicateMethod)
        );
        let bad: HandlerTable = vec![("A b", Box::new(Nop))];
        assert_eq!(
            Dispatcher::from_handlers(bad).err(),
            Some(DispatcherError::InvalidMethodName)
        );
        assert!(Dispatcher::builtin().is_ok());
    }

    mod dispatch {
        use super::*;
        use fandhe_browser_core::AppState;
        use fandhe_browser_profile::Profile;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        struct TempDir(std::path::PathBuf);

        impl TempDir {
            fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let base = fandhe_browser_profile::test_base_dir();
                Self(base.join(format!(
                    "fandhe-cdp-protocol-test-{}-{n}",
                    std::process::id()
                )))
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn state(dir: &TempDir) -> Arc<CdpState> {
            let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
            Arc::new(CdpState::new(Arc::new(AppState::with_disabled_renderer(
                profile,
            ))))
        }

        struct Alpha;
        impl CommandHandler for Alpha {
            fn handle<'a>(
                &'a self,
                _: CommandContext<'a>,
                _: &'a Value,
            ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
                Box::pin(async { Ok(HandlerOutput::result(json!({"which": "alpha"}))) })
            }
        }

        /// params をそのまま返し、イベントを 2 件同伴する。
        struct Echo;
        impl CommandHandler for Echo {
            fn handle<'a>(
                &'a self,
                _: CommandContext<'a>,
                params: &'a Value,
            ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
                Box::pin(async move {
                    let ev = |m| CdpEvent {
                        method: m,
                        params: json!({}),
                        session_id: None,
                    };
                    Ok(HandlerOutput::result(params.clone())
                        .with_event(ev("Test.first"))
                        .with_event(ev("Test.second")))
                })
            }
        }

        struct Fail;
        impl CommandHandler for Fail {
            fn handle<'a>(
                &'a self,
                _: CommandContext<'a>,
                _: &'a Value,
            ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
                Box::pin(async { Err(CdpError::INVALID_PARAMS) })
            }
        }

        fn dispatcher() -> Dispatcher {
            Dispatcher::from_handlers(vec![
                ("Alpha.one", Box::new(Alpha)),
                ("Beta.two", Box::new(Echo)),
                ("Gamma.fail", Box::new(Fail)),
            ])
            .unwrap()
        }

        fn frames(o: DispatchOutcome) -> Vec<Value> {
            o.into_frames()
                .iter()
                .map(|f| serde_json::from_str(f).unwrap())
                .collect()
        }

        #[tokio::test]
        async fn cdp1_dispatch_routes_arbitrary_methods_to_registered_handlers() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = dispatcher();
            let a = frames(d.dispatch(&st, r#"{"id":10,"method":"Alpha.one"}"#).await);
            assert_eq!(a, vec![json!({"id": 10, "result": {"which": "alpha"}})]);
            let b = frames(
                d.dispatch(&st, r#"{"id":11,"method":"Beta.two","params":{"x":[1,2]}}"#)
                    .await,
            );
            assert_eq!(b[0], json!({"id": 11, "result": {"x": [1, 2]}}));
        }

        /// 応答前イベントを 1 件、応答後イベントを 1 件同伴する。
        struct PreAndPost;
        impl CommandHandler for PreAndPost {
            fn handle<'a>(
                &'a self,
                _: CommandContext<'a>,
                _: &'a Value,
            ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
                Box::pin(async {
                    let ev = |m| CdpEvent {
                        method: m,
                        params: json!({}),
                        session_id: None,
                    };
                    Ok(HandlerOutput::result(json!({}))
                        .with_pre_event(ev("Test.pre"))
                        .with_event(ev("Test.post")))
                })
            }
        }

        #[tokio::test]
        async fn cdp5_dispatch_emits_pre_events_before_response() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = Dispatcher::from_handlers(vec![("Gamma.three", Box::new(PreAndPost))]).unwrap();
            let f = frames(
                d.dispatch(&st, r#"{"id":1,"method":"Gamma.three","sessionId":"S-1"}"#)
                    .await,
            );
            assert_eq!(
                f,
                vec![
                    json!({"method": "Test.pre", "params": {}, "sessionId": "S-1"}),
                    json!({"id": 1, "result": {}, "sessionId": "S-1"}),
                    json!({"method": "Test.post", "params": {}, "sessionId": "S-1"}),
                ]
            );
        }
        #[tokio::test]
        async fn cdp1_dispatch_emits_response_then_events_in_order_with_session() {
            let dir = TempDir::new();
            let st = state(&dir);
            let f = frames(
                dispatcher()
                    .dispatch(&st, r#"{"id":1,"method":"Beta.two","sessionId":"S-1"}"#)
                    .await,
            );
            assert_eq!(
                f,
                vec![
                    json!({"id": 1, "result": {}, "sessionId": "S-1"}),
                    json!({"method": "Test.first", "params": {}, "sessionId": "S-1"}),
                    json!({"method": "Test.second", "params": {}, "sessionId": "S-1"}),
                ]
            );
        }

        #[tokio::test]
        async fn cdp1_dispatch_unknown_method_is_error_without_echo() {
            let dir = TempDir::new();
            let st = state(&dir);
            let text = r#"{"id":7,"method":"Evil.secretMethod"}"#;
            let raw = dispatcher()
                .dispatch(&st, text)
                .await
                .into_frames()
                .remove(0);
            assert!(!raw.contains("Evil") && !raw.contains("result"));
            let v: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                v,
                json!({"id": 7, "error": {"code": -32601, "message": "method not implemented"}})
            );
            // 組込み表でも未登録メソッドは同じ。
            let b = Dispatcher::builtin().unwrap();
            let v = frames(
                b.dispatch(&st, r#"{"id":1,"method":"Emulation.setUserAgentOverride"}"#)
                    .await,
            );
            assert_eq!(
                v,
                vec![
                    json!({"id": 1, "error": {"code": -32601, "message": "method not implemented"}})
                ]
            );
        }

        #[tokio::test]
        async fn cdp2_builtin_dispatch_browser_get_version_succeeds() {
            let dir = TempDir::new();
            let st = state(&dir);
            let b = Dispatcher::builtin().unwrap();
            let product = format!("fandhe-browser/{}", env!("CARGO_PKG_VERSION"));
            let v = frames(
                b.dispatch(&st, r#"{"id":1,"method":"Browser.getVersion"}"#)
                    .await,
            );
            assert_eq!(
                v,
                vec![json!({"id": 1, "result": {
                    "protocolVersion": "1.3", "product": product, "userAgent": product
                }})]
            );
        }

        async fn one(st: &Arc<CdpState>, conn: Option<ConnId>, text: &str) -> Value {
            let b = Dispatcher::builtin().unwrap();
            frames(b.dispatch_on(st, conn, text).await).remove(0)
        }

        #[tokio::test]
        async fn cdp2_builtin_get_target_info() {
            let dir = TempDir::new();
            let st = state(&dir);
            let v = one(&st, None, r#"{"id":1,"method":"Target.getTargetInfo"}"#).await;
            assert_eq!(
                v,
                json!({"id": 1, "result": {"targetInfo": {"targetId": st.browser_id().as_str(), "type": "browser", "title": "", "url": "", "attached": true, "canAccessOpener": false}}})
            );
            for bad in [r#""ABC""#, r#""bad id!""#, "1"] {
                let t = format!(
                    r#"{{"id":2,"method":"Target.getTargetInfo","params":{{"targetId":{bad}}}}}"#
                );
                let v = one(&st, None, &t).await;
                assert_eq!(v["error"]["code"], -32602, "{bad}");
            }
            let id = st
                .registry()
                .create_target(crate::target::TargetKind::Page, "about:blank")
                .unwrap();
            let t = format!(
                r#"{{"id":3,"method":"Target.getTargetInfo","params":{{"targetId":"{}"}}}}"#,
                id.as_str()
            );
            let v = one(&st, None, &t).await;
            let ti = &v["result"]["targetInfo"];
            assert_eq!(
                (ti["type"].as_str(), ti["attached"].as_bool()),
                (Some("page"), Some(false))
            );
            st.registry().attach(&id).unwrap();
            let v = one(&st, None, &t).await;
            assert_eq!(v["result"]["targetInfo"]["attached"], true);
        }

        #[tokio::test]
        async fn cdp6_builtin_other_methods_remain_unimplemented() {
            let dir = TempDir::new();
            let st = state(&dir);
            let b = Dispatcher::builtin().unwrap();
            for (i, m) in [
                "Target.disposeBrowserContext",
                "Target.attachToTarget",
                "Runtime.getProperties",
                "Runtime.releaseObjectGroup",
                "Emulation.setUserAgentOverride",
                "Network.setUserAgentOverride",
                "Emulation.setDeviceMetricsOverride",
                "Emulation.setLocaleOverride",
                "Emulation.setTimezoneOverride",
            ]
            .iter()
            .enumerate()
            {
                let text = format!(r#"{{"id":{},"method":"{m}"}}"#, i + 1);
                let v = frames(b.dispatch(&st, &text).await);
                assert_eq!(
                    v,
                    vec![
                        json!({"id": i + 1, "error": {"code": -32601, "message": "method not implemented"}})
                    ],
                    "{m}"
                );
            }
        }

        #[tokio::test]
        async fn cdp6_dispatch_records_unimplemented_and_handled_methods() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = dispatcher();
            let v = frames(
                d.dispatch(&st, r#"{"id":1,"method":"Evil.secretMethod"}"#)
                    .await,
            );
            assert_eq!(
                v,
                vec![
                    json!({"id": 1, "error": {"code": -32601, "message": "method not implemented"}})
                ]
            );
            d.dispatch(&st, r#"{"id":2,"method":"Alpha.one"}"#).await;
            d.dispatch(&st, r#"{"id":3,"method":"Gamma.fail"}"#).await;
            let s = st.received_methods();
            let get = |n: &str| s.methods.iter().find(|(k, _)| k == n).map(|(_, c)| *c);
            let evil = get("Evil.secretMethod").unwrap();
            assert_eq!((evil.handled, evil.unimplemented), (0, 1));
            let alpha = get("Alpha.one").unwrap();
            assert_eq!((alpha.handled, alpha.unimplemented), (1, 0));
            let gamma = get("Gamma.fail").unwrap();
            assert_eq!((gamma.handled, gamma.unimplemented), (1, 0));
            assert_eq!(s.dropped, 0);
        }

        #[tokio::test]
        async fn cdp6_dispatch_does_not_record_parse_failures() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = dispatcher();
            d.dispatch(&st, "not json").await;
            d.dispatch(&st, r#"{"id":3,"method":"A b"}"#).await;
            let s = st.received_methods();
            assert!(s.methods.is_empty());
            assert_eq!(s.dropped, 0);
        }

        #[tokio::test]
        async fn cdp1_dispatch_handler_error_and_parse_error_responses() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = dispatcher();
            let v = frames(d.dispatch(&st, r#"{"id":5,"method":"Gamma.fail"}"#).await);
            assert_eq!(
                v,
                vec![json!({"id": 5, "error": {"code": -32602, "message": "invalid params"}})]
            );
            let v = frames(d.dispatch(&st, "not json").await);
            assert_eq!(
                v,
                vec![json!({"error": {"code": -32700, "message": "parse error"}})]
            );
        }
    }
}
