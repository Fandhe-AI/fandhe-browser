//! 親プロセスと JS 評価用の子プロセスの間で stdio 越しにやり取りする
//! バイナリプロトコルのフレーミング・コーデック（`JS-1`・`TASK-29`・
//! Issue #503「JS 評価の子プロセス分離」設計書 §3.5・§7 W2）。
//!
//! 呼び出し元（将来）: 子プロセスの入口（[`super::worker`]。W3 で追加）と
//! 親側のプロキシ（[`super::process_engine`]。W4 で追加）の双方が本
//! モジュールを経由してフレームを送受信する。**`v8` crate には依存しない**
//! （子・親どちらが読んでも、この段階では V8 固有の型・API を一切参照
//! しない設計にする。テストも `js-v8` feature なしで実行できる）。
//!
//! # フレーム形式
//!
//! `u32 LE len`（タグ 1 バイトを含むペイロード全体のバイト数）＋
//! `u8 tag`（[`tag`] モジュール参照）＋ `payload`（`len - 1` バイト）。
//!
//! # tag の一覧（設計書 §3.5）
//!
//! - [`tag::HELLO`]: 子 → 親。ハンドシェイク（[`encode_hello`]）
//! - [`tag::EVALUATE`]: 親 → 子。評価対象のスクリプト全文
//!   （[`encode_evaluate`]）
//! - [`tag::RESULT`]: 子 → 親。評価結果（[`encode_js_value`]）
//! - [`tag::ERROR`]: 子 → 親。評価失敗（[`encode_error`]）
//! - [`tag::NATIVE_CALL`]: 子 → 親。逆方向 RPC の呼び出し（グローバル関数
//!   注入・DOM 風バインディングの子プロセス対応。`u32 id` ＋ 引数列。
//!   [`encode_native_call`]/[`decode_native_call`]。`JS-1`・`TASK-29`・
//!   Issue #511）
//! - [`tag::NATIVE_RETURN`]: 親 → 子。逆方向 RPC の戻り値
//!   （[`NativeReturn`]。[`encode_native_return`]/[`decode_native_return`]。
//!   Issue #511）。親側の dispatch（`NativeFn` の実行・本フレームの送信。
//!   `super::process_engine::dispatch_native_call`）は Issue #526 で
//!   実装済み
//! - [`tag::SHUTDOWN`]: 親 → 子。ペイロードなし
//! - [`tag::REGISTER_GLOBAL_FUNCTION`]: 親 → 子。グローバル関数の登録
//!   （`u32 id` ＋ name。[`encode_register_global_function`]/
//!   [`decode_register_global_function`]）。応答は `RESULT(Undefined)`
//!   （成功）または `ERROR{Binding}`（登録失敗）。`JS-1`・`TASK-29.4`・
//!   Issue #155
//! - [`tag::BIND_DOM_LIKE_OBJECT`]: 親 → 子。DOM 風オブジェクトの bind
//!   （handle ID・グローバル名・メンバー名と `NativeCall` ID の対応表。
//!   [`encode_bind_dom_like_object`]/[`decode_bind_dom_like_object`]）。
//!   応答は `RESULT(Undefined)`（成功）または `ERROR{Binding}`（失敗）。
//!   親の送信（`process_engine`）・子の処理（`worker`）は実装済み
//!   （`TASK-29.5b`・Issue #525）。`JS-1`・`TASK-29.5a`・Issue #524
//!
//! # 検証方針（外部入力として扱う。coding-rust.md「外部入力」節）
//!
//! 親から見て子は untrusted な JS を実行しているプロセスであり、子から
//! 見ても親からのバイトは信頼できる保証がない（プロトコル自体にバグが
//! あれば壊れたフレームが来うる）。このため本モジュールは双方向とも
//! 「送信側を信頼しない」前提で実装する。
//!
//! - フレーム長は読み取る**前**に上限（[`read_frame`] の `max_payload_len`
//!   引数。呼び出し側が方向ごとの上限を渡す）と比較し、上限を超える場合は
//!   ペイロードを確保する前に `Err` を返す（無制限確保による DoS を防ぐ。
//!   security.md・OWASP A04）
//! - `unwrap`・`expect`・添字アクセス（`[]`）は使わない。`get()`・
//!   `try_into()`・`str::from_utf8` を使う
//! - 未知の tag・不正な UTF-8・想定より短いペイロードは
//!   [`ProtocolError`] として呼び出し側に返す。呼び出し側（`super::worker`・
//!   `super::process_engine`）がこれをプロトコル違反として扱い、相手側の
//!   プロセスを終了させる方針は W3・W4 のドキュメントコメントを参照

use std::io::{self, Read, Write};

use super::engine_trait::{EngineKind, JsValue, ObjectHandle};

/// 子プロセスモードを起動する環境変数名（設計書 §3.1）。値は
/// [`PROTOCOL_VERSION`] と同じ形式（`u16` の文字列表現）のプロトコル
/// バージョンであることを子プロセス（[`super::worker`]）が検証する。
///
/// 呼び出し元（将来）: [`super::run_js_worker_if_requested`]（`lib.rs`）が
/// `js-v8` feature の有無に関わらずこの環境変数の存在を確認する
/// （設計書 §3.1「フックを呼ばないホストへの対策」）。[`super::process_engine`]
/// （W4）はこの環境変数へ [`PROTOCOL_VERSION`] を設定して子プロセスを
/// 起動する。
pub(crate) const MARKER_ENV_VAR: &str = "FANDHE_BROWSER_JS_WORKER";

/// 本 crate が実装しているワーカープロトコルのバージョン（設計書 §3.1）。
/// [`super::worker`]（`js-v8` feature 有効時のみ）が実際に検証・送信に
/// 使う。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
///
/// 2: 親 → 子の登録フレーム [`tag::REGISTER_GLOBAL_FUNCTION`] を追加
/// （`TASK-29.4`・Issue #155）。子は未知の tag をプロトコル違反として
/// 扱うため、バイナリの世代が混ざった場合にハンドシェイクで検出できる
/// ようにバージョンを上げた。
///
/// 3: 値タグ 5（[`JsValue::ObjectHandle`]）と親 → 子の
/// [`tag::BIND_DOM_LIKE_OBJECT`] を追加（`TASK-29.5a`・Issue #524）。
/// `NativeReturn`／`Result` 経由で新しい値タグが実際に流れうるため、
/// ワイヤ形式の世代差をハンドシェイクで検出できるよう上げた。
pub(crate) const PROTOCOL_VERSION: u16 = 3;

/// フレームの `tag` バイトの値。
///
/// `HELLO`/`EVALUATE`/`RESULT`/`ERROR`/`SHUTDOWN` は [`super::worker`]
/// （`js-v8` feature 有効時のみ存在）から使われる。`js-v8` feature が
/// 無効なビルドでは非テスト時に未使用になるため `allow` を宣言する
/// （`expect` ではなく `allow` にする理由: 個々の未配線項目
/// （`decode_js_value` 等）の `expect(dead_code)` と異なるスコープに
/// 独立して付けるための整理。REPAIR-3）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) mod tag {
    /// 子 → 親。ハンドシェイク（[`super::encode_hello`]）。
    pub(crate) const HELLO: u8 = 1;
    /// 親 → 子。評価対象のスクリプト全文（[`super::encode_evaluate`]）。
    pub(crate) const EVALUATE: u8 = 2;
    /// 子 → 親。評価結果（[`super::encode_js_value`]）。
    pub(crate) const RESULT: u8 = 3;
    /// 子 → 親。評価失敗（[`super::encode_error`]）。
    pub(crate) const ERROR: u8 = 4;
    /// 子 → 親。逆方向 RPC の呼び出し（[`super::encode_native_call`]。
    /// `super::v8_engine` のプロキシ関数が送る）。
    pub(crate) const NATIVE_CALL: u8 = 5;
    /// 親 → 子。逆方向 RPC の戻り値（[`super::encode_native_return`]。
    /// 親側の送信は `super::process_engine::dispatch_native_call`
    /// が担う。Issue #526）。
    pub(crate) const NATIVE_RETURN: u8 = 6;
    /// 親 → 子。ペイロードなし。
    pub(crate) const SHUTDOWN: u8 = 7;
    /// 親 → 子。グローバル関数の登録
    /// （[`super::encode_register_global_function`]。`TASK-29.4`・
    /// Issue #155）。応答は `RESULT(Undefined)` / `ERROR{Binding}`。
    pub(crate) const REGISTER_GLOBAL_FUNCTION: u8 = 8;
    /// 親 → 子。DOM 風オブジェクトの bind
    /// （[`super::encode_bind_dom_like_object`]。`TASK-29.5a`・Issue #524）。
    /// 応答は `RESULT(Undefined)` / `ERROR{Binding}`。親の送信は
    /// `process_engine`、子の処理は `worker` が担う（Issue #525）。
    #[cfg_attr(
        not(any(test, feature = "js-v8")),
        allow(
            dead_code,
            reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
        )
    )]
    pub(crate) const BIND_DOM_LIKE_OBJECT: u8 = 9;
}

/// 注入するグローバル関数名の最大バイト数（`JS-1`・`TASK-29.4`・
/// Issue #155）。親の事前検証・登録フレームの decode・子の
/// `v8_engine` が同じ値を共有する。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) const MAX_GLOBAL_FUNCTION_NAME_BYTES: usize = 256;

/// 注入できるグローバル関数の最大件数（親の登録簿・子の登録数の双方で
/// 検証する。子は親を信頼しない。`JS-1`・`TASK-29.4`・Issue #155）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) const MAX_REGISTERED_GLOBAL_FUNCTIONS: usize = 1024;

/// [`super::process_engine`] が親 → 子のフレーム読み取りに使う上限
/// （ペイロードのバイト数。タグ 1 バイトを含まない。設計書 §3.5）。
/// [`super::worker`] のスクリプト長上限（`MAX_SCRIPT_SOURCE_BYTES`。1 MiB）
/// に余裕（64 KiB）を足した値。[`super::worker`] が実際に読み取りへ使う
/// （`js-v8` feature 有効時のみ）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) const MAX_FRAME_PAYLOAD_PARENT_TO_CHILD: usize = 1_048_576 + 65_536;

/// [`super::process_engine`]（W4）が子 → 親のフレーム読み取りに使う上限
/// （ペイロードのバイト数。設計書 §3.5）。結果文字列の上限（1M UTF-16
/// 単位。UTF-8 では最悪 3 MiB）に余裕（64 KiB）を足した値。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) const MAX_FRAME_PAYLOAD_CHILD_TO_PARENT: usize = 3 * 1_048_576 + 65_536;

/// エラーメッセージのペイロードに許容する最大バイト数（4 KiB。設計書
/// §3.5）。[`encode_error`] を呼ぶ側が送信前に切り詰める（本モジュールは
/// 切り詰め自体は行わない。呼び出し元が既存の
/// `MAX_ERROR_MESSAGE_CHARS`/`truncate_error_message` 相当の上限を適用
/// 済みであることを前提とする）。[`super::worker`] が実際に使う
/// （`js-v8` feature 有効時のみ）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) const MAX_ERROR_MESSAGE_BYTES: usize = 4096;

/// [`DomLikeObjectBinding`] のメンバー（メソッド・プロパティ）の最大件数
/// （`JS-1`・`TASK-29.5a`・Issue #524）。DoS 対策として
/// [`decode_bind_dom_like_object`] は `member_count` をこの値と比較してから
/// 確保する。ヘッダ最大 266 B ＋ メンバー最大 263 B × 256 件 ＝ 最大
/// 67,594 B で、[`MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`] に十分収まる。
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
pub(crate) const MAX_DOM_LIKE_OBJECT_MEMBERS: usize = 256;

/// [`DomLikeMember`] の名前の最大バイト数（`JS-1`・`TASK-29.5a`・Issue #524）。
/// グローバル名の上限は [`MAX_GLOBAL_FUNCTION_NAME_BYTES`] を共用する。
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
pub(crate) const MAX_DOM_LIKE_MEMBER_NAME_BYTES: usize = 256;

/// [`encode_native_call`]/[`decode_native_call`] が受け付ける引数の最大件数
/// （`JS-1`・`TASK-29`・Issue #511）。
///
/// DoS 対策の件数上限（OWASP A04「不安全な設計」・security.md）。
/// [`decode_native_call`] は `argc` をこの定数と比較してから
/// `Vec::with_capacity` する（確保前検証。巨大な `argc` を主張するフレーム
/// でも実際にはメモリを確保しない）。`print`・`dom.setText` 等（PoC-3
/// 相当）の用途には十分な値として選んだ。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) const MAX_NATIVE_CALL_ARGS: usize = 64;

/// フレーム・値のデコードに失敗したことを表すエラー（`JS-1`・`TASK-29`・
/// Issue #503）。
///
/// 呼び出し元（`super::worker`・`super::process_engine`）は、この
/// エラーをプロトコル違反として扱い、相手側のプロセスとの通信を打ち切る
/// （設計書 §3.5「プロトコル違反は子を kill する」）。
#[derive(Debug)]
pub(crate) enum ProtocolError {
    /// 下層の I/O エラー（読み取り・書き込み失敗。[`write_frame`]・
    /// [`read_frame`] が返す。`js-v8` feature 有効時のみ非テストで構築
    /// されうる）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    Io(io::Error),
    /// フレーム長が 0（タグバイト自体が存在しない。[`read_frame`]。
    /// `js-v8` feature 有効時のみ非テストで構築されうる）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    EmptyFrame,
    /// ペイロード長が呼び出し側の指定した上限を超えた（[`read_frame`]。
    /// `js-v8` feature 有効時のみ非テストで構築されうる）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    FrameTooLarge { len: usize, max: usize },
    /// 未知の tag バイト。呼び出し元（`super::worker`・
    /// `super::process_engine`）がタグの意味解釈をした結果として構築する
    /// 想定であり、`read_frame` 自体はタグの妥当性を検証しない
    /// （テストのみが構築する。§7 W3 時点では `super::worker` 側の
    /// 不明タグ処理は独自のエラーメッセージで済ませており、本 variant は
    /// 未構築のまま）。
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "呼び出し側がタグの意味解釈をした結果として構築する想定の variant。\
                      現時点では未構築（REPAIR-3）"
        )
    )]
    UnknownTag(u8),
    /// 値のデコード時に、未知の [`JsValue`] タグバイトを受け取った
    /// （[`decode_js_value`]。W4（process_engine.rs）から使われるまで
    /// 未構築）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    InvalidValueTag(u8),
    /// 真偽値の表現が `0`/`1` のどちらでもなかった（[`decode_js_value`]。
    /// W4 から使われるまで未構築）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    InvalidBoolByte(u8),
    /// 未知の [`ErrorKind`] タグバイト（[`decode_error`]。W4 から使われる
    /// まで未構築）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    InvalidErrorKind(u8),
    /// 未知のエンジン種別バイト（`Hello` フレームの `engine` フィールド。
    /// [`decode_hello`]。W4 から使われるまで未構築）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    InvalidEngineByte(u8),
    /// ペイロードが期待する長さに満たない（切り詰められている。
    /// [`decode_js_value`]/[`decode_hello`]/[`decode_error`]。W4 から
    /// 使われるまで未構築）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    Truncated,
    /// ペイロードが期待する長さを超えている（末尾に余分なバイトが
    /// 付いている。[`decode_hello`] のように、フレーム全体を 1 つの
    /// 固定長値として解釈する箇所で構築する。codex レビュー指摘 #503
    /// P1「decode_hello が末尾の余分なバイトを拒否していない」対応）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    TrailingBytes { expected: usize, actual: usize },
    /// 文字列として解釈すべきバイト列が不正な UTF-8 だった
    /// （[`decode_evaluate`]。`super::worker` が親からの `Evaluate`
    /// フレームを検証する際に構築する。`js-v8` feature 有効時のみ非
    /// テストで構築されうる）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    InvalidUtf8,
    /// [`encode_native_call`] に渡した引数の件数が [`MAX_NATIVE_CALL_ARGS`]
    /// を超えていた（`JS-1`・Issue #511）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    TooManyNativeCallArgs { count: usize, max: usize },
    /// [`decode_native_return`] が未知の status バイト（0=Ok, 1=Err 以外）を
    /// 受け取った（`JS-1`・Issue #511）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    InvalidNativeReturnStatus(u8),
    /// [`decode_native_return`] の Err ペイロード（エラーメッセージ）が
    /// [`MAX_ERROR_MESSAGE_BYTES`] を超えていた（`JS-1`・Issue #511）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    ErrorMessageTooLong { len: usize, max: usize },
    /// [`encode_native_call`] の引数列をエンコードした場合の合計サイズが
    /// [`MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`] を超える（`JS-1`・Issue #511・
    /// codex レビュー指摘 #533 P0）。
    ///
    /// 引数の**件数**は [`MAX_NATIVE_CALL_ARGS`]（[`TooManyNativeCallArgs`]）
    /// で別途検証済みだが、1 引数あたり最大 ~1 MiB の文字列を渡せるため、
    /// 件数の検証だけでは合計バイト数の DoS を防げない。`encode_native_call`
    /// は `out` へ追記する**前**に合計サイズを checked 演算で積算し、この
    /// 上限を超えた時点で確保・追記を一切行わずに本エラーを返す。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    NativeCallPayloadTooLarge { size: usize, max: usize },
    /// [`decode_register_global_function`] の name が
    /// [`MAX_GLOBAL_FUNCTION_NAME_BYTES`] を超えていた（`TASK-29.4`・
    /// Issue #155）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    GlobalFunctionNameTooLong { len: usize, max: usize },
    /// bind フレームのメンバー数が [`MAX_DOM_LIKE_OBJECT_MEMBERS`] を超えた
    /// （`TASK-29.5a`・Issue #524）。
    #[cfg_attr(
        not(any(test, feature = "js-v8")),
        allow(
            dead_code,
            reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
        )
    )]
    TooManyDomLikeMembers { count: usize, max: usize },
    /// bind フレームのグローバル名・メンバー名が上限を超えた
    /// （`TASK-29.5a`・Issue #524）。
    #[cfg_attr(
        not(any(test, feature = "js-v8")),
        allow(
            dead_code,
            reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
        )
    )]
    DomLikeNameTooLong { len: usize, max: usize },
    /// bind フレームのグローバル名・メンバー名が空（`TASK-29.5a`・Issue #524）。
    #[cfg_attr(
        not(any(test, feature = "js-v8")),
        allow(
            dead_code,
            reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
        )
    )]
    EmptyDomLikeName,
    /// bind フレームのメンバー種別バイトが未知（`TASK-29.5a`・Issue #524）。
    #[cfg_attr(
        not(any(test, feature = "js-v8")),
        allow(
            dead_code,
            reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
        )
    )]
    InvalidDomLikeMemberKind(u8),
    /// bind フレームでメンバー名が重複した（`TASK-29.5a`・Issue #524）。
    /// untrusted な名前をメッセージへ埋め込まないため名前は保持しない。
    #[cfg_attr(
        not(any(test, feature = "js-v8")),
        allow(
            dead_code,
            reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
        )
    )]
    DuplicateDomLikeMemberName,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "worker protocol I/O error: {err}"),
            Self::EmptyFrame => write!(f, "worker protocol frame has zero length"),
            Self::FrameTooLarge { len, max } => write!(
                f,
                "worker protocol frame payload of {len} bytes exceeds the {max}-byte limit"
            ),
            Self::UnknownTag(tag) => write!(f, "worker protocol frame has unknown tag {tag}"),
            Self::InvalidValueTag(tag) => write!(f, "worker protocol value has unknown tag {tag}"),
            Self::InvalidBoolByte(byte) => {
                write!(f, "worker protocol boolean byte {byte} is neither 0 nor 1")
            }
            Self::InvalidErrorKind(byte) => {
                write!(f, "worker protocol error kind byte {byte} is unknown")
            }
            Self::InvalidEngineByte(byte) => {
                write!(f, "worker protocol engine byte {byte} is unknown")
            }
            Self::Truncated => write!(f, "worker protocol payload is truncated"),
            Self::TrailingBytes { expected, actual } => write!(
                f,
                "worker protocol payload has {actual} bytes but only {expected} were expected \
                 (trailing bytes)"
            ),
            Self::InvalidUtf8 => write!(f, "worker protocol payload is not valid UTF-8"),
            Self::TooManyNativeCallArgs { count, max } => write!(
                f,
                "worker protocol NativeCall has {count} arguments, exceeding the {max}-argument limit"
            ),
            Self::InvalidNativeReturnStatus(byte) => write!(
                f,
                "worker protocol NativeReturn status byte {byte} is neither 0 (Ok) nor 1 (Err)"
            ),
            Self::ErrorMessageTooLong { len, max } => write!(
                f,
                "worker protocol NativeReturn error message of {len} bytes exceeds the \
                 {max}-byte limit"
            ),
            Self::NativeCallPayloadTooLarge { size, max } => write!(
                f,
                "worker protocol NativeCall payload of {size} bytes exceeds the {max}-byte limit"
            ),
            Self::GlobalFunctionNameTooLong { len, max } => write!(
                f,
                "worker protocol global function name of {len} bytes exceeds the {max}-byte limit"
            ),
            Self::TooManyDomLikeMembers { count, max } => write!(
                f,
                "worker protocol DOM-like object has {count} members, exceeding the {max}-member limit"
            ),
            Self::DomLikeNameTooLong { len, max } => write!(
                f,
                "worker protocol DOM-like object name of {len} bytes exceeds the {max}-byte limit"
            ),
            Self::EmptyDomLikeName => {
                write!(f, "worker protocol DOM-like object name must not be empty")
            }
            Self::InvalidDomLikeMemberKind(byte) => write!(
                f,
                "worker protocol DOM-like member kind byte {byte} is unknown"
            ),
            Self::DuplicateDomLikeMemberName => write!(
                f,
                "worker protocol DOM-like object has a duplicate member name"
            ),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// [`read_frame`] の内部処理: `buf` を完全に埋めるまで読み込む。
///
/// 通常の `Read::read_exact` と異なり、**1 バイトも読めないまま EOF に
/// 達した場合**（＝フレームの区切りとして正当な EOF）と、**一部だけ読めた
/// 状態で EOF に達した場合**（＝フレームの途中で相手が終了した異常な
/// 状態）を区別して返す。前者は `Ok(false)`、後者は
/// [`io::ErrorKind::UnexpectedEof`] の `Err` にする。
///
/// [`read_frame`] からのみ呼ばれる（`js-v8` feature 有効時のみ非テストで
/// 到達する）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
fn read_exact_or_clean_eof(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "worker protocol stream ended in the middle of a frame",
                ));
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(true)
}

/// 1 フレームを書き出す（`u32 LE len` ＋ `tag` ＋ `payload`）。
///
/// 呼び出し側（`super::worker`・`super::process_engine`）がフレームごとに
/// `flush` することを想定し、本関数自体は `flush` しない（複数フレームを
/// まとめて `flush` したい呼び出し側の裁量に委ねる）。`js-v8` feature
/// 有効時のみ非テストで [`super::worker`] から使われる。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn write_frame(
    writer: &mut impl Write,
    tag: u8,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    // タグ 1 バイト分を含めた長さを u32 として送る。呼び出し側が渡す
    // ペイロードは本モジュールの `MAX_FRAME_PAYLOAD_*` 定数を超えない
    // 前提（呼び出し元が送信前に検証する）だが、念のため `u32` へ収まる
    // ことを検査してから書き込む（無検証のキャストによる切り捨てを防ぐ。
    // coding-rust.md「外部入力」節）。
    let len_with_tag = payload
        .len()
        .checked_add(1)
        .and_then(|len| u32::try_from(len).ok())
        .ok_or_else(|| {
            ProtocolError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker protocol payload is too large to encode a u32 frame length",
            ))
        })?;
    writer.write_all(&len_with_tag.to_le_bytes())?;
    writer.write_all(&[tag])?;
    writer.write_all(payload)?;
    Ok(())
}

/// 1 フレームを読み取る。
///
/// 戻り値:
/// - `Ok(Some((tag, payload)))`: フレームを 1 つ読み取れた
/// - `Ok(None)`: フレームの区切り（長さプレフィックスの先頭）で相手が
///   ストリームを閉じた（正常終了。設計書 §3.2「stdin を閉じる。子は
///   EOF を受けて正常に終了する」に対応する）
/// - `Err(_)`: フレームの途中で終了した、長さが `max_payload_len` を
///   超えた、その他の I/O エラー
///
/// `max_payload_len` は呼び出し側が方向ごとに渡す（親から子は
/// [`MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`]、子から親は
/// [`MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`]）。ペイロードを確保する**前**に
/// 検査するため、上限超過を主張するフレームに対して実際のメモリ確保は
/// 発生しない（OWASP A04「不安全な設計」対策）。`js-v8` feature 有効時
/// のみ非テストで [`super::worker`] から使われる。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn read_frame(
    reader: &mut impl Read,
    max_payload_len: usize,
) -> Result<Option<(u8, Vec<u8>)>, ProtocolError> {
    let mut len_buf = [0u8; 4];
    if !read_exact_or_clean_eof(reader, &mut len_buf)? {
        return Ok(None);
    }
    let len_with_tag = u32::from_le_bytes(len_buf) as usize;
    if len_with_tag == 0 {
        return Err(ProtocolError::EmptyFrame);
    }
    let payload_len = len_with_tag - 1;
    if payload_len > max_payload_len {
        return Err(ProtocolError::FrameTooLarge {
            len: payload_len,
            max: max_payload_len,
        });
    }

    let mut tag_buf = [0u8; 1];
    reader.read_exact(&mut tag_buf)?;

    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;

    Ok(Some((tag_buf[0], payload)))
}

/// [`JsValue`] を、[`tag::RESULT`] フレームのペイロード（や将来の
/// `NativeCall` 引数列）として使えるバイト列へ追記する。
///
/// 表現: `u8 tag`（0=Undefined, 1=Null, 2=Bool, 3=Number, 4=String,
/// 5=ObjectHandle）＋ 種別ごとの値（Bool は `u8`、Number は `f64 LE`、
/// String は `u32 LE len` ＋ UTF-8 バイト列、ObjectHandle は `u32 LE id`）。
///
/// `String` を自己区切り（長さ明示）にしているため、将来 `Vec<JsValue>` を
/// 連結して送る際（`NativeCall`）にもそのまま再利用できる。
///
/// `JsValue` は `#[non_exhaustive]` だが、本 crate の内側からの `match` は
/// 既知の全 variant を網羅すれば `_` 分岐は不要（`#[non_exhaustive]` が
/// 制限するのは他 crate からの網羅性判定のみ）。`js-v8` feature 有効時
/// のみ非テストで [`super::worker`] から使われる。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn encode_js_value(value: &JsValue, out: &mut Vec<u8>) -> Result<(), ProtocolError> {
    match value {
        JsValue::Undefined => out.push(0),
        JsValue::Null => out.push(1),
        JsValue::Bool(b) => {
            out.push(2);
            out.push(u8::from(*b));
        }
        JsValue::Number(n) => {
            out.push(3);
            out.extend_from_slice(&n.to_le_bytes());
        }
        JsValue::String(s) => {
            out.push(4);
            let len = u32::try_from(s.len()).map_err(|_| {
                ProtocolError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "worker protocol string value is too large to encode a u32 length",
                ))
            })?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        JsValue::ObjectHandle(handle) => {
            out.push(5);
            out.extend_from_slice(&handle.raw().to_le_bytes());
        }
    }
    Ok(())
}

/// [`encode_js_value`] の逆変換。`buf` の先頭から 1 つの [`JsValue`] を
/// 読み取り、値と消費したバイト数を返す（将来 `Vec<JsValue>` を連結して
/// 読む際に、消費量から次の値の開始位置を計算できるようにするため）。
///
/// 呼び出し元（将来）: `super::process_engine`（W4）が子からの `Result`
/// フレームをデコードする際に使う。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn decode_js_value(buf: &[u8]) -> Result<(JsValue, usize), ProtocolError> {
    let tag = *buf.first().ok_or(ProtocolError::Truncated)?;
    match tag {
        0 => Ok((JsValue::Undefined, 1)),
        1 => Ok((JsValue::Null, 1)),
        2 => {
            let byte = *buf.get(1).ok_or(ProtocolError::Truncated)?;
            match byte {
                0 => Ok((JsValue::Bool(false), 2)),
                1 => Ok((JsValue::Bool(true), 2)),
                other => Err(ProtocolError::InvalidBoolByte(other)),
            }
        }
        3 => {
            let bytes: [u8; 8] = buf
                .get(1..9)
                .ok_or(ProtocolError::Truncated)?
                .try_into()
                .map_err(|_| ProtocolError::Truncated)?;
            Ok((JsValue::Number(f64::from_le_bytes(bytes)), 9))
        }
        4 => {
            let len_bytes: [u8; 4] = buf
                .get(1..5)
                .ok_or(ProtocolError::Truncated)?
                .try_into()
                .map_err(|_| ProtocolError::Truncated)?;
            let len = u32::from_le_bytes(len_bytes) as usize;
            let start: usize = 5;
            let end = start.checked_add(len).ok_or(ProtocolError::Truncated)?;
            let str_bytes = buf.get(start..end).ok_or(ProtocolError::Truncated)?;
            let s = std::str::from_utf8(str_bytes).map_err(|_| ProtocolError::InvalidUtf8)?;
            Ok((JsValue::String(s.to_string()), end))
        }
        5 => {
            let id_bytes: [u8; 4] = buf
                .get(1..5)
                .ok_or(ProtocolError::Truncated)?
                .try_into()
                .map_err(|_| ProtocolError::Truncated)?;
            Ok((
                JsValue::ObjectHandle(ObjectHandle::from_raw(u32::from_le_bytes(id_bytes))),
                5,
            ))
        }
        other => Err(ProtocolError::InvalidValueTag(other)),
    }
}

/// [`tag::HELLO`] フレームのペイロードを組み立てる。
///
/// 表現: `u16 LE protocol_version` ＋ `u8 engine`（0=V8, 1=Boa）。
/// `js-v8` feature 有効時のみ非テストで [`super::worker`] から使われる。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn encode_hello(protocol_version: u16, engine: EngineKind) -> Vec<u8> {
    let mut out = Vec::with_capacity(3);
    out.extend_from_slice(&protocol_version.to_le_bytes());
    out.push(match engine {
        EngineKind::V8 => 0,
        EngineKind::Boa => 1,
    });
    out
}

/// [`encode_hello`] の逆変換。
///
/// 呼び出し元（将来）: `super::process_engine`（W4）がハンドシェイクで
/// 子からの `Hello` フレームをデコードする際に使う。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn decode_hello(payload: &[u8]) -> Result<(u16, EngineKind), ProtocolError> {
    // `Hello` は常にちょうど 3 バイト（`u16` の version＋`u8` の engine）の
    // 固定長フレームであり、他の値が後ろに連結される設計ではない。
    // 末尾に余分なバイトがあれば黙って無視せず拒否する（codex レビュー
    // 指摘 #503 P1「decode_hello が末尾の余分なバイトを拒否していない」
    // 対応。`decode_js_value` のように複数値の連結読み取りに使う関数とは
    // 契約が異なる）。
    const HELLO_PAYLOAD_LEN: usize = 3;
    if payload.len() < HELLO_PAYLOAD_LEN {
        return Err(ProtocolError::Truncated);
    }
    if payload.len() > HELLO_PAYLOAD_LEN {
        return Err(ProtocolError::TrailingBytes {
            expected: HELLO_PAYLOAD_LEN,
            actual: payload.len(),
        });
    }
    let version_bytes: [u8; 2] = payload
        .get(0..2)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .map_err(|_| ProtocolError::Truncated)?;
    let version = u16::from_le_bytes(version_bytes);
    let engine_byte = *payload.get(2).ok_or(ProtocolError::Truncated)?;
    let engine = match engine_byte {
        0 => EngineKind::V8,
        1 => EngineKind::Boa,
        other => return Err(ProtocolError::InvalidEngineByte(other)),
    };
    Ok((version, engine))
}

/// [`tag::EVALUATE`] フレームのペイロードを組み立てる（スクリプト全文の
/// UTF-8 バイト列そのもの。フレーム長で境界が定まるため追加の長さ
/// プレフィックスは持たない）。
///
/// 呼び出し元（将来）: `super::process_engine`（W4）が子へ評価対象の
/// スクリプトを送る際に使う。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn encode_evaluate(script: &str) -> Vec<u8> {
    script.as_bytes().to_vec()
}

/// [`encode_evaluate`] の逆変換。`js-v8` feature 有効時のみ非テストで
/// [`super::worker`] から使われる（親からの `Evaluate` フレームを検証
/// する経路）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn decode_evaluate(payload: &[u8]) -> Result<String, ProtocolError> {
    std::str::from_utf8(payload)
        .map(str::to_string)
        .map_err(|_| ProtocolError::InvalidUtf8)
}

/// [`tag::ERROR`] フレームが表す失敗の種別（`JS-1`・Issue #503 設計書
/// §3.3）。
///
/// 呼び出し元（`super::process_engine`）はこれを
/// [`super::engine_trait::JsEngineError`] へ変換する（W5 で対応表を実装
/// する）。子プロセス側（`super::worker`。`js-v8` feature 有効時のみ）は
/// 既に評価失敗の分類にこの型を使っている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) enum ErrorKind {
    /// スクリプト評価が失敗した（構文エラー・実行時例外）。
    Evaluation,
    /// グローバル関数・DOM 風オブジェクトの登録に失敗した
    /// （[`JsEngineError::BindingFailed`] に対応）。`TASK-29.4`
    /// （Issue #155）から、[`tag::REGISTER_GLOBAL_FUNCTION`] への応答として
    /// 子が返す（登録に失敗しても子と Context は生きている）。DOM 風
    /// オブジェクトの登録は `TASK-29.5` で同じ種別を使う想定。
    Binding,
    /// 子の監視スレッド（watchdog）が実行時間の上限で打ち切った
    /// （設計書 §3.3 の表「子の watchdog が打ち切った」行）。
    Timeout,
}

impl ErrorKind {
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    fn to_byte(self) -> u8 {
        match self {
            Self::Evaluation => 0,
            Self::Binding => 1,
            Self::Timeout => 2,
        }
    }

    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    fn from_byte(byte: u8) -> Result<Self, ProtocolError> {
        match byte {
            0 => Ok(Self::Evaluation),
            1 => Ok(Self::Binding),
            2 => Ok(Self::Timeout),
            other => Err(ProtocolError::InvalidErrorKind(other)),
        }
    }
}

/// [`tag::ERROR`] フレームのペイロードを組み立てる。
///
/// 表現: `u8 kind`（[`ErrorKind::to_byte`]）＋ `message` の UTF-8 バイト列
/// （フレーム長で境界が定まる）。呼び出し側が送信前に
/// [`MAX_ERROR_MESSAGE_BYTES`] へ切り詰め済みであることを前提とする
/// （本関数自体は切り詰めない）。`js-v8` feature 有効時のみ非テストで
/// [`super::worker`] から使われる。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn encode_error(kind: ErrorKind, message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + message.len());
    out.push(kind.to_byte());
    out.extend_from_slice(message.as_bytes());
    out
}

/// [`encode_error`] の逆変換。
///
/// 呼び出し元（将来）: `super::process_engine`（W4）が子からの `Error`
/// フレームをデコードする際に使う。
///
/// **末尾の余分なバイトを拒否する必要が無い**（codex レビュー指摘 #503
/// P1「decode_js_value が返す消費バイト数を捨てている」と同じ観点での
/// 確認。`decode_hello`・`decode_js_value` の呼び出し元とは異なり対応は
/// 不要）: `message` は「`kind` の 1 バイトより後ろの残り全部」として
/// 定義されており（長さプレフィックスを持たない）、`payload.len()` を
/// 常に過不足なく消費する。末尾に余分なバイトが付く余地自体が無い
/// （不正な内容であれば `InvalidUtf8` として拒否される）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn decode_error(payload: &[u8]) -> Result<(ErrorKind, String), ProtocolError> {
    let kind_byte = *payload.first().ok_or(ProtocolError::Truncated)?;
    let kind = ErrorKind::from_byte(kind_byte)?;
    let message_bytes = payload.get(1..).ok_or(ProtocolError::Truncated)?;
    let message = std::str::from_utf8(message_bytes).map_err(|_| ProtocolError::InvalidUtf8)?;
    Ok((kind, message.to_string()))
}

/// [`tag::REGISTER_GLOBAL_FUNCTION`] フレームのペイロードを組み立てる
/// （親 → 子。`JS-1`・`TASK-29.4`・Issue #155）。
///
/// 表現: `u32 LE id` ＋ `name` の UTF-8 バイト列（残り全部。長さは
/// フレーム長で決まる）。呼び出し元: `super::process_engine` の
/// `register_one`（`js-v8` feature 有効時のみ）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn encode_register_global_function(id: u32, name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + name.len());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

/// [`encode_register_global_function`] の逆変換（子側の
/// `super::worker` が使う。送信側を信頼しない）。
///
/// 4 バイト未満・不正 UTF-8・空の name・[`MAX_GLOBAL_FUNCTION_NAME_BYTES`]
/// 超の name は [`ProtocolError`] にする。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn decode_register_global_function(
    payload: &[u8],
) -> Result<(u32, String), ProtocolError> {
    let id_bytes: [u8; 4] = payload
        .get(0..4)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .map_err(|_| ProtocolError::Truncated)?;
    let id = u32::from_le_bytes(id_bytes);
    let name_bytes = payload.get(4..).ok_or(ProtocolError::Truncated)?;
    if name_bytes.is_empty() {
        return Err(ProtocolError::Truncated);
    }
    if name_bytes.len() > MAX_GLOBAL_FUNCTION_NAME_BYTES {
        return Err(ProtocolError::GlobalFunctionNameTooLong {
            len: name_bytes.len(),
            max: MAX_GLOBAL_FUNCTION_NAME_BYTES,
        });
    }
    let name = std::str::from_utf8(name_bytes).map_err(|_| ProtocolError::InvalidUtf8)?;
    Ok((id, name.to_string()))
}

/// [`DomLikeMember`] の種別（`JS-1`・`TASK-29.5a`・Issue #524）。
///
/// setter（書き込み可能なプロパティ）は未対応で、将来の kind 値として
/// 予約するのみ（REPAIR-3。デコードは未知の kind として拒否する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
pub(crate) enum DomLikeMemberKind {
    /// メソッド。呼び出しごとに `NativeCall` を送る。
    Method,
    /// 読み取り専用プロパティ（getter）。参照ごとに `NativeCall` を送る。
    Property,
}

impl DomLikeMemberKind {
    #[cfg_attr(
        not(any(test, feature = "js-v8")),
        allow(
            dead_code,
            reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
        )
    )]
    fn to_byte(self) -> u8 {
        match self {
            Self::Method => 0,
            Self::Property => 1,
        }
    }

    #[cfg_attr(
        not(any(test, feature = "js-v8")),
        allow(
            dead_code,
            reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
        )
    )]
    fn from_byte(byte: u8) -> Result<Self, ProtocolError> {
        match byte {
            0 => Ok(Self::Method),
            1 => Ok(Self::Property),
            other => Err(ProtocolError::InvalidDomLikeMemberKind(other)),
        }
    }
}

/// bind する DOM 風オブジェクトの 1 メンバー（`JS-1`・`TASK-29.5a`・Issue #524）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
pub(crate) struct DomLikeMember {
    /// メンバー種別。
    pub(crate) kind: DomLikeMemberKind,
    /// JS 側に見せる名前。空不可・[`MAX_DOM_LIKE_MEMBER_NAME_BYTES`] 以下・
    /// オブジェクト内で一意。子は JS のソース文字列へ連結せず
    /// `create_data_property` 等の API で設定すること（インジェクション対策）。
    pub(crate) name: String,
    /// 呼び出し時に `NativeCall` で送る ID。handle ID とは別の名前空間。
    pub(crate) native_call_id: u32,
}

/// [`tag::BIND_DOM_LIKE_OBJECT`] フレームの内容（`JS-1`・`TASK-29.5a`・
/// Issue #524）。
///
/// 親（`super::process_engine`。Issue #525 で配線済み）が子へ送り、子の
/// `super::worker` が受けて JS のグローバルへプロキシオブジェクトを生やす。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
pub(crate) struct DomLikeObjectBinding {
    /// 親側 bind 済みオブジェクトの handle。
    pub(crate) handle: ObjectHandle,
    /// グローバルに生やす名前。空不可・[`MAX_GLOBAL_FUNCTION_NAME_BYTES`] 以下。
    pub(crate) name: String,
    /// メンバー一覧（[`MAX_DOM_LIKE_OBJECT_MEMBERS`] 件以下）。
    pub(crate) members: Vec<DomLikeMember>,
}

/// bind フレームの名前 1 件を検証する（空・長さ超過を拒否）。
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
fn validate_dom_like_name(name: &str, max: usize) -> Result<(), ProtocolError> {
    if name.is_empty() {
        return Err(ProtocolError::EmptyDomLikeName);
    }
    if name.len() > max {
        return Err(ProtocolError::DomLikeNameTooLong {
            len: name.len(),
            max,
        });
    }
    Ok(())
}

/// [`tag::BIND_DOM_LIKE_OBJECT`] フレームのペイロードを組み立てる
/// （親 → 子。`JS-1`・`TASK-29.5a`・Issue #524）。
///
/// 表現（すべて LE）: `u32 handle_id` ＋ `u16 name_len` ＋ name ＋
/// `u32 member_count` ＋ メンバー × N（`u8 kind`〔0=Method, 1=Property〕＋
/// `u32 native_call_id` ＋ `u16 name_len` ＋ name）。
///
/// 空の名前・上限超過・メンバー名の重複は、確保前に検証して
/// [`ProtocolError`] を返す。
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
pub(crate) fn encode_bind_dom_like_object(
    binding: &DomLikeObjectBinding,
) -> Result<Vec<u8>, ProtocolError> {
    validate_dom_like_name(&binding.name, MAX_GLOBAL_FUNCTION_NAME_BYTES)?;
    if binding.members.len() > MAX_DOM_LIKE_OBJECT_MEMBERS {
        return Err(ProtocolError::TooManyDomLikeMembers {
            count: binding.members.len(),
            max: MAX_DOM_LIKE_OBJECT_MEMBERS,
        });
    }
    let member_count =
        u32::try_from(binding.members.len()).map_err(|_| ProtocolError::TooManyDomLikeMembers {
            count: binding.members.len(),
            max: MAX_DOM_LIKE_OBJECT_MEMBERS,
        })?;

    let mut seen = std::collections::HashSet::with_capacity(binding.members.len());
    // handle(4) + name_len(2) + name + member_count(4)
    let mut total_size: usize = 4 + 2 + binding.name.len() + 4;
    for member in &binding.members {
        validate_dom_like_name(&member.name, MAX_DOM_LIKE_MEMBER_NAME_BYTES)?;
        if !seen.insert(member.name.as_str()) {
            return Err(ProtocolError::DuplicateDomLikeMemberName);
        }
        // kind(1) + native_call_id(4) + name_len(2) + name
        total_size = total_size
            .checked_add(1 + 4 + 2 + member.name.len())
            .ok_or(ProtocolError::NativeCallPayloadTooLarge {
                size: usize::MAX,
                max: MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
            })?;
    }
    if total_size > MAX_FRAME_PAYLOAD_PARENT_TO_CHILD {
        return Err(ProtocolError::NativeCallPayloadTooLarge {
            size: total_size,
            max: MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
        });
    }

    let mut out = Vec::with_capacity(total_size);
    out.extend_from_slice(&binding.handle.raw().to_le_bytes());
    push_dom_like_name(&mut out, &binding.name)?;
    out.extend_from_slice(&member_count.to_le_bytes());
    for member in &binding.members {
        out.push(member.kind.to_byte());
        out.extend_from_slice(&member.native_call_id.to_le_bytes());
        push_dom_like_name(&mut out, &member.name)?;
    }
    Ok(out)
}

/// `u16 LE len` ＋ UTF-8 バイト列を追記する（長さは検証済みの前提だが
/// `u16` に収まらなければ拒否する）。
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
fn push_dom_like_name(out: &mut Vec<u8>, name: &str) -> Result<(), ProtocolError> {
    let len = u16::try_from(name.len()).map_err(|_| ProtocolError::DomLikeNameTooLong {
        len: name.len(),
        max: usize::from(u16::MAX),
    })?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    Ok(())
}

/// [`encode_bind_dom_like_object`] の逆変換（子側の `super::worker` が使う。
/// 送信側を信頼しない）。
///
/// `member_count` を上限と比較してから `Vec::with_capacity` する。切り詰め・
/// 不正 UTF-8・空の名前・長さ超過・未知の kind・メンバー名の重複・末尾の
/// 余分なバイトはすべて [`ProtocolError`] にする。
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
pub(crate) fn decode_bind_dom_like_object(
    payload: &[u8],
) -> Result<DomLikeObjectBinding, ProtocolError> {
    let mut reader = DomLikeReader {
        buf: payload,
        pos: 0,
    };
    let handle = ObjectHandle::from_raw(reader.read_u32()?);
    let name = reader.read_name(MAX_GLOBAL_FUNCTION_NAME_BYTES)?;
    let member_count = reader.read_u32()? as usize;
    if member_count > MAX_DOM_LIKE_OBJECT_MEMBERS {
        return Err(ProtocolError::TooManyDomLikeMembers {
            count: member_count,
            max: MAX_DOM_LIKE_OBJECT_MEMBERS,
        });
    }
    let mut members = Vec::with_capacity(member_count);
    let mut seen = std::collections::HashSet::with_capacity(member_count);
    for _ in 0..member_count {
        let kind = DomLikeMemberKind::from_byte(reader.read_u8()?)?;
        let native_call_id = reader.read_u32()?;
        let member_name = reader.read_name(MAX_DOM_LIKE_MEMBER_NAME_BYTES)?;
        if !seen.insert(member_name.clone()) {
            return Err(ProtocolError::DuplicateDomLikeMemberName);
        }
        members.push(DomLikeMember {
            kind,
            name: member_name,
            native_call_id,
        });
    }
    if reader.pos != payload.len() {
        return Err(ProtocolError::TrailingBytes {
            expected: reader.pos,
            actual: payload.len(),
        });
    }
    Ok(DomLikeObjectBinding {
        handle,
        name,
        members,
    })
}

/// [`decode_bind_dom_like_object`] 専用の境界検査付きバイト読み取り。
#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
struct DomLikeReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[cfg_attr(
    not(any(test, feature = "js-v8")),
    allow(
        dead_code,
        reason = "js-v8 feature 有効時のみ子の worker が使う（TASK-29.5b・REPAIR-3）"
    )
)]
impl DomLikeReader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], ProtocolError> {
        let end = self.pos.checked_add(n).ok_or(ProtocolError::Truncated)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(ProtocolError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, ProtocolError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(ProtocolError::Truncated)
    }

    fn read_u32(&mut self) -> Result<u32, ProtocolError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| ProtocolError::Truncated)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_name(&mut self, max: usize) -> Result<String, ProtocolError> {
        let len_bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| ProtocolError::Truncated)?;
        let len = usize::from(u16::from_le_bytes(len_bytes));
        if len == 0 {
            return Err(ProtocolError::EmptyDomLikeName);
        }
        if len > max {
            return Err(ProtocolError::DomLikeNameTooLong { len, max });
        }
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| ProtocolError::InvalidUtf8)
    }
}

/// [`tag::NATIVE_CALL`] フレームのペイロードを組み立てる（子 → 親。`JS-1`・
/// `TASK-29`・Issue #511）。
///
/// 表現: `u32 LE id` ＋ `u32 LE argc` ＋ `argc` 個の [`JsValue`]
/// （[`encode_js_value`] の自己区切り形式を連結したもの）。
///
/// 呼び出し元: [`super::v8_engine`] のプロキシ関数コールバック
/// （`native_proxy_callback`）が、子プロセス内で JS からホスト関数が呼ばれた
/// ことを親へ伝える際に使う。`args.len()` が [`MAX_NATIVE_CALL_ARGS`] を
/// 超える場合は送信前に `Err` を返す（DoS 対策。確保前検証は呼び出し側の
/// 責務ではなく、本関数自体が引数個数を検証してから追記する）。
///
/// 件数の検証に加え、`out` へ追記する**前**に各引数のエンコード後サイズを
/// [`encoded_js_value_len`] で checked 演算により積算し、
/// [`MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`]（`NativeCall` は子 → 親フレーム）
/// を超えた時点で確保・追記を一切行わずに `Err` を返す（codex レビュー
/// 指摘 #533 P0「サイズ検証前に全件確保している」対応。呼び出し元
/// （[`super::worker::StdioTransport::call`]）は本関数が返した `Vec` の
/// 長さを送信直前に再確認するが、その時点ではすでに確保が終わっている
/// ため、DoS 対策としては確保前の本検証が主たる防御線になる）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn encode_native_call(id: u32, args: &[JsValue]) -> Result<Vec<u8>, ProtocolError> {
    if args.len() > MAX_NATIVE_CALL_ARGS {
        return Err(ProtocolError::TooManyNativeCallArgs {
            count: args.len(),
            max: MAX_NATIVE_CALL_ARGS,
        });
    }
    let argc = u32::try_from(args.len()).map_err(|_| ProtocolError::TooManyNativeCallArgs {
        count: args.len(),
        max: MAX_NATIVE_CALL_ARGS,
    })?;

    // `id`（4 バイト）＋ `argc`（4 バイト）のヘッダ分から積算を始め、
    // 各引数のエンコード後サイズを `Vec::with_capacity` の前に確定させる。
    let mut total_size: usize = 8;
    for arg in args {
        let encoded_len = encoded_js_value_len(arg)?;
        total_size = total_size.checked_add(encoded_len).ok_or(
            ProtocolError::NativeCallPayloadTooLarge {
                size: usize::MAX,
                max: MAX_FRAME_PAYLOAD_CHILD_TO_PARENT,
            },
        )?;
        if total_size > MAX_FRAME_PAYLOAD_CHILD_TO_PARENT {
            return Err(ProtocolError::NativeCallPayloadTooLarge {
                size: total_size,
                max: MAX_FRAME_PAYLOAD_CHILD_TO_PARENT,
            });
        }
    }

    let mut out = Vec::with_capacity(total_size);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&argc.to_le_bytes());
    for arg in args {
        encode_js_value(arg, &mut out)?;
    }
    Ok(out)
}

/// [`encode_js_value`] が `value` に対して実際に追記するバイト数を、
/// 確保・追記の**前**に計算する（[`encode_native_call`] が合計サイズを
/// 検証してから `Vec::with_capacity` するために使う補助関数。`JS-1`・
/// Issue #511・codex レビュー指摘 #533）。
///
/// 表現は [`encode_js_value`] のドキュメントコメントと一致させる
/// （`u8 tag` ＋ 種別ごとの値）。`String` の長さ検証（`u32` に収まるか）も
/// [`encode_js_value`] と同じ基準で行う。
///
/// `pub(crate)`: [`super::v8_engine`] の `native_call_arg_encoded_len`
/// （複製を伴わない V8 側の見積もり。codex レビュー指摘 #533 P0 対応）の
/// ドキュメンテーションコメントから参照する。表現形式（タグ 1 バイト＋
/// 種別ごとの値）は手作業で同期する必要がある。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn encoded_js_value_len(value: &JsValue) -> Result<usize, ProtocolError> {
    match value {
        JsValue::Undefined | JsValue::Null => Ok(1),
        JsValue::Bool(_) => Ok(2),
        JsValue::Number(_) => Ok(9),
        JsValue::ObjectHandle(_) => Ok(5),
        JsValue::String(s) => {
            u32::try_from(s.len()).map_err(|_| {
                ProtocolError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "worker protocol string value is too large to encode a u32 length",
                ))
            })?;
            1usize
                .checked_add(4)
                .and_then(|n| n.checked_add(s.len()))
                .ok_or(ProtocolError::NativeCallPayloadTooLarge {
                    size: usize::MAX,
                    max: MAX_FRAME_PAYLOAD_CHILD_TO_PARENT,
                })
        }
    }
}

/// [`encode_native_call`] の逆変換（親側で使う。`JS-1`・Issue #511）。
///
/// `argc` を [`MAX_NATIVE_CALL_ARGS`] と比較してから `Vec::with_capacity`
/// する（外部入力である `argc` を信用してそのまま確保に使わない。
/// coding-rust.md「外部入力」節・OWASP A04）。読み終えて末尾にバイトが
/// 余っていれば [`ProtocolError::TrailingBytes`] を返す。
///
/// 呼び出し元: `super::process_engine::dispatch_native_call`（`js-v8`
/// feature 有効時のみ）。Issue #526 で実装済み。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn decode_native_call(payload: &[u8]) -> Result<(u32, Vec<JsValue>), ProtocolError> {
    let id_bytes: [u8; 4] = payload
        .get(0..4)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .map_err(|_| ProtocolError::Truncated)?;
    let id = u32::from_le_bytes(id_bytes);

    let argc_bytes: [u8; 4] = payload
        .get(4..8)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .map_err(|_| ProtocolError::Truncated)?;
    let argc = u32::from_le_bytes(argc_bytes) as usize;
    if argc > MAX_NATIVE_CALL_ARGS {
        return Err(ProtocolError::TooManyNativeCallArgs {
            count: argc,
            max: MAX_NATIVE_CALL_ARGS,
        });
    }

    let mut args = Vec::with_capacity(argc);
    let mut offset = 8usize;
    for _ in 0..argc {
        let remaining = payload.get(offset..).ok_or(ProtocolError::Truncated)?;
        let (value, consumed) = decode_js_value(remaining)?;
        args.push(value);
        offset = offset
            .checked_add(consumed)
            .ok_or(ProtocolError::Truncated)?;
    }
    if offset != payload.len() {
        return Err(ProtocolError::TrailingBytes {
            expected: offset,
            actual: payload.len(),
        });
    }
    Ok((id, args))
}

/// [`tag::NATIVE_RETURN`] フレームが表す逆方向 RPC の戻り値（親 → 子。
/// `JS-1`・`TASK-29`・Issue #511）。
///
/// 呼び出し元: `super::process_engine::dispatch_native_call`（`Ok` は
/// `NativeFn` の戻り値、`Err` は `NativeFn` の実行失敗・呼び出し規約違反を
/// 表す。Issue #526）。子側（[`super::v8_engine`] のプロキシ関数）は
/// [`decode_native_return`] でこれを読み、`Ok` は JS の戻り値へ、`Err` は
/// JS の例外（`try`/`catch` で捕捉可能）へ変換する。
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) enum NativeReturn {
    /// `NativeFn` の呼び出しが成功した。
    Ok(JsValue),
    /// `NativeFn` の呼び出しが失敗した（エラーメッセージ）。
    Err(String),
}

/// [`encode_native_return`] が `value` に対して実際に追記するバイト数を、
/// 確保・追記の**前**に計算する（[`super::process_engine::encode_bounded_native_return`]
/// が、上限超過を確保してから検出するのではなく、確保**前**に上限超過を
/// 検出できるようにするための補助関数。[`encoded_js_value_len`] と同じ
/// 考え方を `NativeReturn` に適用する。`JS-1`・`TASK-29`・Issue #526・
/// codex レビュー指摘「`NativeReturn` のサイズを確保前に検証する」対応）。
///
/// 表現は [`encode_native_return`] のドキュメントコメントと一致させる
/// （`u8 status` ＋ 本体）。`Ok` は `1（status バイト）+
/// encoded_js_value_len(value)`、`Err` は `1（status バイト）+
/// message.len()` を、どちらも `checked_add` で計算する
/// （`NativeReturn::Err` の `String` は事前に
/// [`truncate_for_wire`]（[`MAX_ERROR_MESSAGE_BYTES`] 以内）を通す前提の
/// ため現実的にはオーバーフローしないが、外部入力由来のサイズ計算である
/// ため `checked_add` を使う。coding-rust.md「外部入力」節）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn encoded_native_return_len(value: &NativeReturn) -> Result<usize, ProtocolError> {
    match value {
        NativeReturn::Ok(value) => {
            let value_len = encoded_js_value_len(value)?;
            1usize
                .checked_add(value_len)
                .ok_or(ProtocolError::NativeCallPayloadTooLarge {
                    size: usize::MAX,
                    max: MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
                })
        }
        NativeReturn::Err(message) => {
            1usize
                .checked_add(message.len())
                .ok_or(ProtocolError::NativeCallPayloadTooLarge {
                    size: usize::MAX,
                    max: MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
                })
        }
    }
}

/// [`NativeReturn`] のペイロードを組み立てる（親側で使う。`JS-1`・
/// Issue #511）。
///
/// 表現: `u8 status`（0=Ok, 1=Err）＋ 本体（Ok は 1 つの [`JsValue`]、Err は
/// 残り全部を UTF-8 のエラーメッセージとして扱う）。
///
/// 呼び出し元: `super::process_engine::dispatch_native_call`。送信前に
/// [`super::worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`] 以内・
/// エラーメッセージを [`MAX_ERROR_MESSAGE_BYTES`] 以内に収める責務は
/// 呼び出し側が負う（本関数自体は上限を課さない。呼び出し側は
/// [`encoded_native_return_len`] で確保前に長さを検証してから本関数を
/// 呼ぶ。`dispatch_native_call`・`encode_bounded_native_return` の
/// ドキュメントコメント参照）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn encode_native_return(value: &NativeReturn) -> Result<Vec<u8>, ProtocolError> {
    let mut out = Vec::new();
    match value {
        NativeReturn::Ok(value) => {
            out.push(0);
            encode_js_value(value, &mut out)?;
        }
        NativeReturn::Err(message) => {
            out.push(1);
            out.extend_from_slice(message.as_bytes());
        }
    }
    Ok(out)
}

/// エラーメッセージを [`MAX_ERROR_MESSAGE_BYTES`] まで切り詰める（バイト
/// 単位。文字境界を壊さないよう `str::is_char_boundary` で境界を探してから
/// 切る。coding-rust.md「外部入力」節）。
///
/// 呼び出し元: [`super::worker`]（子側。評価失敗メッセージを `Error`
/// フレームで送る前）と、[`super::process_engine`] の親側 dispatch
/// （`NativeFn` が `Err` を返した場合の `NativeReturn::Err` を組み立てる
/// 前。`JS-1`・`TASK-29`・Issue #526）の双方が使う共通の切り詰め処理
/// （元は `worker.rs` に private 実装として存在した。両者が使う
/// プロトコル上の制約であるため本モジュールへ移した）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn truncate_for_wire(message: &str) -> String {
    if message.len() <= MAX_ERROR_MESSAGE_BYTES {
        return message.to_string();
    }
    let mut end = MAX_ERROR_MESSAGE_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.get(..end).unwrap_or_default().to_string()
}

/// [`encode_native_return`] の逆変換（子側で使う。`JS-1`・Issue #511）。
///
/// 呼び出し元: [`super::v8_engine`] のプロキシ関数コールバックが、親からの
/// 応答を JS の戻り値・例外へ変換する前に呼ぶ。
///
/// - `status = 0`（Ok）: ペイロードの残り全体をちょうど 1 つの [`JsValue`]
///   として消費すること。末尾に余分なバイトがあれば
///   [`ProtocolError::TrailingBytes`] を返す
/// - `status = 1`（Err）: 残り全部を UTF-8 のエラーメッセージとして読む。
///   [`MAX_ERROR_MESSAGE_BYTES`] を超える場合は `from_utf8` を試みる**前**に
///   [`ProtocolError::ErrorMessageTooLong`] を返す（外部入力の長さを検証
///   してから文字列変換に使う。coding-rust.md「外部入力」節）
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
pub(crate) fn decode_native_return(payload: &[u8]) -> Result<NativeReturn, ProtocolError> {
    let status = *payload.first().ok_or(ProtocolError::Truncated)?;
    let body = payload.get(1..).ok_or(ProtocolError::Truncated)?;
    match status {
        0 => {
            let (value, consumed) = decode_js_value(body)?;
            if consumed != body.len() {
                return Err(ProtocolError::TrailingBytes {
                    expected: consumed,
                    actual: body.len(),
                });
            }
            Ok(NativeReturn::Ok(value))
        }
        1 => {
            if body.len() > MAX_ERROR_MESSAGE_BYTES {
                return Err(ProtocolError::ErrorMessageTooLong {
                    len: body.len(),
                    max: MAX_ERROR_MESSAGE_BYTES,
                });
            }
            let message = std::str::from_utf8(body).map_err(|_| ProtocolError::InvalidUtf8)?;
            Ok(NativeReturn::Err(message.to_string()))
        }
        other => Err(ProtocolError::InvalidNativeReturnStatus(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// JS-1・TASK-29・Issue #503: `write_frame`/`read_frame` の往復で
    /// タグ・ペイロードが一致すること。
    #[test]
    fn js_1_frame_round_trip_preserves_tag_and_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::EVALUATE, b"1 + 1").expect("write must succeed");

        let mut cursor = Cursor::new(buf);
        let (read_tag, payload) = read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD)
            .expect("read must succeed")
            .expect("frame must be present");
        assert_eq!(read_tag, tag::EVALUATE);
        assert_eq!(payload, b"1 + 1");
    }

    /// JS-1: ストリームの先頭（フレーム境界）で EOF になった場合は
    /// `Ok(None)` を返すこと（正常終了の合図として区別できる）。
    #[test]
    fn js_1_read_frame_returns_none_on_clean_eof() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let result = read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD)
            .expect("clean eof must not be an error");
        assert!(result.is_none());
    }

    /// JS-1: フレームの途中（長さプレフィックスの一部だけ）で EOF に
    /// なった場合は `Err` を返すこと（正常終了と区別する）。
    #[test]
    fn js_1_read_frame_errors_on_eof_mid_frame() {
        let mut cursor = Cursor::new(vec![1u8, 2u8]); // 4 バイトの長さに満たない
        let result = read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD);
        assert!(matches!(result, Err(ProtocolError::Io(_))));
    }

    /// JS-1・codex レビュー想定の回帰確認: ペイロード長が上限を超える
    /// フレームは、ペイロードを確保する前に `Err` を返すこと（OWASP A04
    /// 対策の検証）。
    #[test]
    fn js_1_read_frame_rejects_oversized_payload_before_allocating() {
        let max = 16usize;
        let oversized_len_with_tag = u32::try_from(max + 2).expect("fits in u32");
        let mut buf = Vec::new();
        buf.extend_from_slice(&oversized_len_with_tag.to_le_bytes());
        // タグ・ペイロードは書かない（上限検査が確保より前に働くことの
        // 確認。もし実装が誤ってペイロード分を読もうとすれば、ここで
        // 別のエラー（EOF）になり、期待する `FrameTooLarge` にならない
        // ことでも検知できる）。
        let mut cursor = Cursor::new(buf);
        match read_frame(&mut cursor, max) {
            Err(ProtocolError::FrameTooLarge { len, max: max_seen }) => {
                assert_eq!(len, max + 1);
                assert_eq!(max_seen, max);
            }
            other => panic!("expected FrameTooLarge, got: {other:?}"),
        }
    }

    /// JS-1: 長さが 0（タグバイトすら無い）フレームは `Err` を返すこと。
    #[test]
    fn js_1_read_frame_rejects_zero_length_frame() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes());
        let mut cursor = Cursor::new(buf);
        assert!(matches!(
            read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD),
            Err(ProtocolError::EmptyFrame)
        ));
    }

    /// JS-1: 未知の tag バイトは呼び出し側（`read_frame` の戻り値を見た
    /// `super::worker`/`super::process_engine`）が判別できるよう、
    /// `read_frame` 自体はタグの妥当性を検証せずそのまま返すこと
    /// （tag の意味解釈は呼び出し側の責務であることの確認）。
    #[test]
    fn js_1_read_frame_passes_through_unknown_tag_for_caller_to_reject() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 200, b"").expect("write must succeed");
        let mut cursor = Cursor::new(buf);
        let (read_tag, payload) = read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD)
            .expect("read must succeed")
            .expect("frame must be present");
        assert_eq!(read_tag, 200);
        assert!(payload.is_empty());
    }

    /// JS-1: `JsValue` の全 variant が `encode_js_value`/`decode_js_value`
    /// の往復で一致すること。
    #[test]
    fn js_1_js_value_round_trip_for_all_variants() {
        let values = [
            JsValue::Undefined,
            JsValue::Null,
            JsValue::Bool(true),
            JsValue::Bool(false),
            JsValue::Number(42.5),
            JsValue::Number(-1.0),
            JsValue::String(String::new()),
            JsValue::String("fandhe-browser".to_string()),
            JsValue::ObjectHandle(ObjectHandle::from_raw(0)),
            JsValue::ObjectHandle(ObjectHandle::from_raw(u32::MAX)),
        ];
        for value in values {
            let mut buf = Vec::new();
            encode_js_value(&value, &mut buf).expect("encode must succeed");
            let (decoded, consumed) = decode_js_value(&buf).expect("decode must succeed");
            assert_eq!(decoded, value);
            assert_eq!(consumed, buf.len());
        }
    }

    /// JS-1: 複数の `JsValue` を連結したバッファから、消費バイト数を
    /// 使って順番に読み出せること（将来の `NativeCall` 引数列を想定した
    /// 契約の検証）。
    #[test]
    fn js_1_js_value_sequence_can_be_decoded_using_consumed_length() {
        let mut buf = Vec::new();
        encode_js_value(&JsValue::Number(1.0), &mut buf).expect("encode must succeed");
        encode_js_value(&JsValue::String("two".to_string()), &mut buf)
            .expect("encode must succeed");
        encode_js_value(&JsValue::Bool(true), &mut buf).expect("encode must succeed");

        let mut offset = 0;
        let (first, consumed) = decode_js_value(&buf[offset..]).expect("decode must succeed");
        assert_eq!(first, JsValue::Number(1.0));
        offset += consumed;
        let (second, consumed) = decode_js_value(&buf[offset..]).expect("decode must succeed");
        assert_eq!(second, JsValue::String("two".to_string()));
        offset += consumed;
        let (third, consumed) = decode_js_value(&buf[offset..]).expect("decode must succeed");
        assert_eq!(third, JsValue::Bool(true));
        offset += consumed;
        assert_eq!(offset, buf.len());
    }

    /// JS-1: 未知の値タグは `Err(InvalidValueTag)` を返すこと。
    #[test]
    fn js_1_decode_js_value_rejects_unknown_tag() {
        assert!(matches!(
            decode_js_value(&[9]),
            Err(ProtocolError::InvalidValueTag(9))
        ));
    }

    /// JS-1: 空のバッファは `Err(Truncated)` を返すこと（添字アクセスで
    /// panic しないことの確認）。
    #[test]
    fn js_1_decode_js_value_rejects_empty_buffer() {
        assert!(matches!(
            decode_js_value(&[]),
            Err(ProtocolError::Truncated)
        ));
    }

    /// JS-1: 文字列値の長さプレフィックスが実際のバッファより大きい場合、
    /// 添字アクセスで panic せず `Err(Truncated)` を返すこと。
    #[test]
    fn js_1_decode_js_value_rejects_string_with_length_exceeding_buffer() {
        let mut buf = vec![4u8]; // String タグ
        buf.extend_from_slice(&100u32.to_le_bytes()); // 実際には無い長さ
        buf.extend_from_slice(b"short");
        assert!(matches!(
            decode_js_value(&buf),
            Err(ProtocolError::Truncated)
        ));
    }

    /// JS-1: 不正な UTF-8 バイト列は `Err(InvalidUtf8)` を返すこと。
    #[test]
    fn js_1_decode_js_value_rejects_invalid_utf8_string() {
        let mut buf = vec![4u8];
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&[0xff, 0xfe]);
        assert!(matches!(
            decode_js_value(&buf),
            Err(ProtocolError::InvalidUtf8)
        ));
    }

    /// JS-1: `Hello` フレームの往復（`EngineKind::V8`/`Boa` の双方）。
    #[test]
    fn js_1_hello_round_trip() {
        for engine in [EngineKind::V8, EngineKind::Boa] {
            let payload = encode_hello(1, engine);
            let (version, decoded_engine) = decode_hello(&payload).expect("decode must succeed");
            assert_eq!(version, 1);
            assert_eq!(decoded_engine, engine);
        }
    }

    /// JS-1: `Hello` フレームの未知エンジンバイトは `Err` を返すこと。
    #[test]
    fn js_1_decode_hello_rejects_unknown_engine_byte() {
        let payload = [1u8, 0u8, 99u8];
        assert!(matches!(
            decode_hello(&payload),
            Err(ProtocolError::InvalidEngineByte(99))
        ));
    }

    /// codex レビュー指摘 #503 P1「decode_hello が末尾の余分なバイトを
    /// 拒否していない」の単体テスト: 3 バイトより短いペイロードは
    /// `Truncated`、3 バイトより長い（末尾に余分なバイトが付いた）
    /// ペイロードは `TrailingBytes` として明示的に拒否されること
    /// （黙って先頭 3 バイトだけを解釈して残りを無視しないことの確認）。
    #[test]
    fn js_1_decode_hello_rejects_wrong_length_payloads() {
        let too_short = encode_hello(1, EngineKind::V8);
        let too_short = &too_short[..too_short.len() - 1];
        assert!(
            matches!(decode_hello(too_short), Err(ProtocolError::Truncated)),
            "a payload shorter than the fixed 3-byte Hello must be rejected as Truncated"
        );

        let mut too_long = encode_hello(1, EngineKind::V8);
        too_long.push(0xff);
        assert!(
            matches!(
                decode_hello(&too_long),
                Err(ProtocolError::TrailingBytes {
                    expected: 3,
                    actual: 4,
                })
            ),
            "a payload longer than the fixed 3-byte Hello must be rejected as TrailingBytes, \
             got: {:?}",
            decode_hello(&too_long)
        );
    }

    /// JS-1: `Evaluate` フレームの往復。
    #[test]
    fn js_1_evaluate_round_trip() {
        let script = "while (true) {}";
        let payload = encode_evaluate(script);
        let decoded = decode_evaluate(&payload).expect("decode must succeed");
        assert_eq!(decoded, script);
    }

    /// JS-1: `Error` フレームの往復（`ErrorKind` の全 variant）。
    #[test]
    fn js_1_error_round_trip_for_all_kinds() {
        for kind in [
            ErrorKind::Evaluation,
            ErrorKind::Binding,
            ErrorKind::Timeout,
        ] {
            let payload = encode_error(kind, "boom");
            let (decoded_kind, message) = decode_error(&payload).expect("decode must succeed");
            assert_eq!(decoded_kind, kind);
            assert_eq!(message, "boom");
        }
    }

    /// JS-1: `Error` フレームの未知 kind バイトは `Err` を返すこと。
    #[test]
    fn js_1_decode_error_rejects_unknown_kind() {
        let payload = [200u8, b'x'];
        assert!(matches!(
            decode_error(&payload),
            Err(ProtocolError::InvalidErrorKind(200))
        ));
    }

    /// JS-1: 各 `ProtocolError` variant の `Display` 実装が空文字列を
    /// 返さないこと。`UnknownTag` は `read_frame` 自体は使わず、呼び出し側
    /// （`super::worker`・`super::process_engine`。W3・W4）がタグの意味
    /// 解釈をした結果として構築する想定の variant であるため、ここで
    /// `Display` の存在を確認しておく。
    #[test]
    fn js_1_protocol_error_display_messages_are_non_empty() {
        let errors: Vec<ProtocolError> = vec![
            ProtocolError::Io(io::Error::other("boom")),
            ProtocolError::EmptyFrame,
            ProtocolError::FrameTooLarge { len: 10, max: 5 },
            ProtocolError::UnknownTag(9),
            ProtocolError::TooManyDomLikeMembers {
                count: 300,
                max: 256,
            },
            ProtocolError::DomLikeNameTooLong { len: 300, max: 256 },
            ProtocolError::EmptyDomLikeName,
            ProtocolError::InvalidDomLikeMemberKind(9),
            ProtocolError::DuplicateDomLikeMemberName,
            ProtocolError::InvalidValueTag(9),
            ProtocolError::InvalidBoolByte(9),
            ProtocolError::InvalidErrorKind(9),
            ProtocolError::InvalidEngineByte(9),
            ProtocolError::Truncated,
            ProtocolError::InvalidUtf8,
            ProtocolError::TooManyNativeCallArgs { count: 10, max: 5 },
            ProtocolError::InvalidNativeReturnStatus(9),
            ProtocolError::ErrorMessageTooLong { len: 10, max: 5 },
            ProtocolError::NativeCallPayloadTooLarge { size: 10, max: 5 },
        ];
        for err in errors {
            assert!(!err.to_string().is_empty());
        }
    }

    /// JS-1・Issue #511: `NativeCall` の往復（0 引数・複数引数・`id` が
    /// `u32::MAX` のケースを含む）で、id・引数列が一致すること。
    #[test]
    fn js_1_native_call_round_trip() {
        let cases: Vec<(u32, Vec<JsValue>)> = vec![
            (0, vec![]),
            (
                7,
                vec![
                    JsValue::Number(1.0),
                    JsValue::String("x".to_string()),
                    JsValue::Bool(true),
                    JsValue::Null,
                    JsValue::Undefined,
                ],
            ),
            (u32::MAX, vec![JsValue::Number(-1.0)]),
        ];
        for (id, args) in cases {
            let encoded = encode_native_call(id, &args).expect("encode must succeed");
            let (decoded_id, decoded_args) =
                decode_native_call(&encoded).expect("decode must succeed");
            assert_eq!(decoded_id, id);
            assert_eq!(decoded_args, args);
        }
    }

    /// JS-1・Issue #511: 引数の件数が上限を超える場合、`encode_native_call`
    /// は `Err` を返すこと。
    #[test]
    fn js_1_encode_native_call_rejects_too_many_args() {
        let args: Vec<JsValue> = (0..(MAX_NATIVE_CALL_ARGS + 1))
            .map(|_| JsValue::Undefined)
            .collect();
        assert!(matches!(
            encode_native_call(1, &args),
            Err(ProtocolError::TooManyNativeCallArgs { .. })
        ));
    }

    /// codex レビュー指摘 #533 P0: 引数の件数は `MAX_NATIVE_CALL_ARGS`
    /// 以内でも、合計サイズが `MAX_FRAME_PAYLOAD_CHILD_TO_PARENT` を
    /// 超える場合は `encode_native_call` が確保前に `Err` を返すこと。
    /// （1 引数あたり約 1 MiB の文字列を複数渡し、件数の上限には収まるが
    /// 合計サイズの上限は超える状況を再現する。）
    #[test]
    fn js_1_encode_native_call_rejects_oversized_payload_before_building_output() {
        // 1 MiB の文字列を 4 個（計 4 MiB）渡す。件数（4）は
        // `MAX_NATIVE_CALL_ARGS`（64）以内だが、合計サイズは
        // `MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`（3 MiB + 64 KiB）を超える。
        let one_mib_string = "a".repeat(1024 * 1024);
        let args: Vec<JsValue> = (0..4)
            .map(|_| JsValue::String(one_mib_string.clone()))
            .collect();
        match encode_native_call(1, &args) {
            Err(ProtocolError::NativeCallPayloadTooLarge { size, max }) => {
                assert!(size > MAX_FRAME_PAYLOAD_CHILD_TO_PARENT);
                assert_eq!(max, MAX_FRAME_PAYLOAD_CHILD_TO_PARENT);
            }
            other => panic!("expected NativeCallPayloadTooLarge, got {other:?}"),
        }
    }

    /// JS-1・Issue #511・OWASP A04: `argc` が `MAX_NATIVE_CALL_ARGS` を
    /// 超えると主張するフレームは、実際には確保する前に `Err` を返すこと。
    /// `argc = u32::MAX` を主張しつつ本体を一切持たないペイロードで検証する
    /// （実装が誤って `Vec::with_capacity(u32::MAX)` を呼べば OOM で abort
    /// するため、正常終了すること自体が「確保前に検証した」ことの証拠になる）。
    #[test]
    fn js_1_decode_native_call_rejects_oversized_argc_before_allocating() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes()); // id
        payload.extend_from_slice(&u32::MAX.to_le_bytes()); // argc（主張のみ）
        match decode_native_call(&payload) {
            Err(ProtocolError::TooManyNativeCallArgs { count, max }) => {
                assert_eq!(count, u32::MAX as usize);
                assert_eq!(max, MAX_NATIVE_CALL_ARGS);
            }
            other => panic!("expected TooManyNativeCallArgs, got: {other:?}"),
        }
    }

    /// JS-1・Issue #511: 末尾に余分なバイトが付いた `NativeCall` ペイロードは
    /// `Err(TrailingBytes)` を返すこと。
    #[test]
    fn js_1_decode_native_call_rejects_trailing_bytes() {
        let mut payload = encode_native_call(1, &[JsValue::Bool(true)]).expect("encode");
        payload.push(0xff);
        assert!(matches!(
            decode_native_call(&payload),
            Err(ProtocolError::TrailingBytes { .. })
        ));
    }

    /// JS-1・Issue #511: 引数列が途中で切り詰められた `NativeCall` ペイロードは
    /// `Err(Truncated)` を返すこと（添字アクセスで panic しないことの確認）。
    #[test]
    fn js_1_decode_native_call_rejects_truncated_args() {
        let mut payload = encode_native_call(1, &[JsValue::Number(1.0)]).expect("encode");
        payload.truncate(payload.len() - 1);
        assert!(matches!(
            decode_native_call(&payload),
            Err(ProtocolError::Truncated)
        ));
    }

    /// JS-1・Issue #511: `NativeReturn::Ok`/`Err` の往復（マルチバイトの
    /// エラーメッセージを含む）。
    #[test]
    fn js_1_native_return_round_trip() {
        let ok = NativeReturn::Ok(JsValue::String("結果".to_string()));
        let encoded = encode_native_return(&ok).expect("encode must succeed");
        assert_eq!(decode_native_return(&encoded).expect("decode"), ok);

        let err = NativeReturn::Err("エラーが発生しました".to_string());
        let encoded = encode_native_return(&err).expect("encode must succeed");
        assert_eq!(decode_native_return(&encoded).expect("decode"), err);
    }

    /// `TASK-29`・Issue #526・codex レビュー指摘「`NativeReturn` のサイズを
    /// 確保前に検証する」対応: [`encoded_native_return_len`] が
    /// [`encode_native_return`] の実際の出力長と一致すること（`Ok`・`Err`
    /// の両方）。呼び出し元（`super::process_engine::encode_bounded_native_return`）
    /// は、この一致を前提に「符号化する前に上限超過を判定できる」ため、
    /// 2 つの計算が乖離しないことを回帰確認する。
    #[test]
    fn js_1_encoded_native_return_len_matches_encode_native_return_for_ok_and_err() {
        let cases = vec![
            NativeReturn::Ok(JsValue::Undefined),
            NativeReturn::Ok(JsValue::Null),
            NativeReturn::Ok(JsValue::Bool(true)),
            NativeReturn::Ok(JsValue::Number(42.0)),
            NativeReturn::Ok(JsValue::String("結果".to_string())),
            NativeReturn::Err(String::new()),
            NativeReturn::Err("エラーが発生しました".to_string()),
        ];
        for case in cases {
            let expected_len = encode_native_return(&case)
                .unwrap_or_else(|err| panic!("encode must succeed for {case:?}: {err}"))
                .len();
            let computed_len = encoded_native_return_len(&case).unwrap_or_else(|err| {
                panic!("length computation must succeed for {case:?}: {err}")
            });
            assert_eq!(
                computed_len, expected_len,
                "encoded_native_return_len must match encode_native_return's actual output \
                 length for {case:?}"
            );
        }
    }

    /// JS-1・Issue #511: `decode_native_return` の拒否ケース（未知の
    /// status・Ok の末尾に余分なバイト・不正な UTF-8・上限を超える
    /// メッセージ・空のペイロード）。
    #[test]
    fn js_1_decode_native_return_rejects_invalid_payloads() {
        assert!(matches!(
            decode_native_return(&[9]),
            Err(ProtocolError::InvalidNativeReturnStatus(9))
        ));
        assert!(matches!(
            decode_native_return(&[]),
            Err(ProtocolError::Truncated)
        ));

        let mut ok_with_trailing =
            encode_native_return(&NativeReturn::Ok(JsValue::Bool(true))).expect("encode");
        ok_with_trailing.push(0xff);
        assert!(matches!(
            decode_native_return(&ok_with_trailing),
            Err(ProtocolError::TrailingBytes { .. })
        ));

        let mut invalid_utf8 = vec![1u8];
        invalid_utf8.extend_from_slice(&[0xff, 0xfe]);
        assert!(matches!(
            decode_native_return(&invalid_utf8),
            Err(ProtocolError::InvalidUtf8)
        ));

        let mut too_long = vec![1u8];
        too_long.extend(std::iter::repeat_n(b'a', MAX_ERROR_MESSAGE_BYTES + 1));
        assert!(matches!(
            decode_native_return(&too_long),
            Err(ProtocolError::ErrorMessageTooLong { .. })
        ));
    }

    /// JS-1・TASK-29.4: 登録件数・名前長の上限が具体値であること（親子で共有）。
    #[test]
    fn js_1_global_function_limits_have_the_documented_values() {
        assert_eq!(MAX_REGISTERED_GLOBAL_FUNCTIONS, 1024);
        assert_eq!(MAX_GLOBAL_FUNCTION_NAME_BYTES, 256);
    }

    /// JS-1・TASK-29.4: 登録フレームが往復し、id と name が具体値で戻ること。
    #[test]
    fn js_1_register_global_function_round_trips() {
        let payload = encode_register_global_function(7, "hostAdd");
        assert_eq!(&payload[..4], &7u32.to_le_bytes());
        assert_eq!(
            decode_register_global_function(&payload).expect("decode"),
            (7, "hostAdd".to_string())
        );
    }

    /// JS-1・TASK-29.4: 4 バイト未満・空の name は拒否する。
    #[test]
    fn js_1_register_global_function_rejects_truncated_and_empty_name() {
        assert!(matches!(
            decode_register_global_function(&[1, 0, 0]),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            decode_register_global_function(&[]),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            decode_register_global_function(&1u32.to_le_bytes()),
            Err(ProtocolError::Truncated)
        ));
    }

    /// JS-1・TASK-29.4: 不正 UTF-8 は拒否する。
    #[test]
    fn js_1_register_global_function_rejects_invalid_utf8() {
        let mut payload = 1u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&[0xff, 0xfe]);
        assert!(matches!(
            decode_register_global_function(&payload),
            Err(ProtocolError::InvalidUtf8)
        ));
    }

    /// JS-1・TASK-29.4: name はちょうど 256 バイトまで通り、257 バイトは拒否する。
    #[test]
    fn js_1_register_global_function_enforces_name_length_limit() {
        let ok = "a".repeat(MAX_GLOBAL_FUNCTION_NAME_BYTES);
        assert_eq!(
            decode_register_global_function(&encode_register_global_function(0, &ok))
                .expect("256 bytes is allowed")
                .1,
            ok
        );
        let long = "a".repeat(MAX_GLOBAL_FUNCTION_NAME_BYTES + 1);
        assert!(matches!(
            decode_register_global_function(&encode_register_global_function(0, &long)),
            Err(ProtocolError::GlobalFunctionNameTooLong { len: 257, max: 256 })
        ));
    }

    /// JS-1: エラーメッセージのペイロード上限が 4 KiB であること（設計書
    /// §3.5 の具体値の回帰確認）。
    #[test]
    fn js_1_max_error_message_bytes_is_4_kib() {
        assert_eq!(MAX_ERROR_MESSAGE_BYTES, 4096);
    }

    /// JS-1: 方向ごとのフレーム上限が設計書 §3.5 の具体値
    /// （親→子: 1 MiB + 64 KiB、子→親: 3 MiB + 64 KiB）と一致すること。
    /// `MAX_FRAME_PAYLOAD_CHILD_TO_PARENT` は W3・W4（子・親の実装）が
    /// `read_frame` へ渡す上限であり、本モジュール単体では他に使用箇所が
    /// ないため、ここで具体値を固定する回帰テストとして参照する。
    #[test]
    fn js_1_max_frame_payload_limits_match_the_design_doc() {
        assert_eq!(
            MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
            1_048_576 + 65_536,
            "parent-to-child limit must be 1 MiB + 64 KiB"
        );
        assert_eq!(
            MAX_FRAME_PAYLOAD_CHILD_TO_PARENT,
            3 * 1_048_576 + 65_536,
            "child-to-parent limit must be 3 MiB + 64 KiB"
        );
    }

    /// JS-1・Issue #503: 上限以内のメッセージはそのまま返すこと
    /// （`worker.rs` から移設。`TASK-29`・Issue #526）。
    #[test]
    fn js_1_truncate_for_wire_keeps_short_messages_untouched() {
        assert_eq!(truncate_for_wire("boom"), "boom");
    }

    /// JS-1・Issue #503: 上限を超えるメッセージは `MAX_ERROR_MESSAGE_BYTES`
    /// 以下（かつ文字境界で切られたバイト列）に切り詰められること
    /// （`worker.rs` から移設。`TASK-29`・Issue #526）。
    #[test]
    fn js_1_truncate_for_wire_truncates_oversized_messages_at_a_char_boundary() {
        // マルチバイト文字（3 バイトの日本語）を境界ちょうどに配置し、
        // 境界探索が正しく機能することを確認する。
        let message = "a".repeat(MAX_ERROR_MESSAGE_BYTES - 1) + "あ" + "b";
        let truncated = truncate_for_wire(&message);
        assert!(truncated.len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    /// JS-1: 環境変数名・プロトコルバージョンの具体値（設計書 §3.1）。
    #[test]
    fn js_1_marker_env_var_and_protocol_version_have_the_documented_values() {
        assert_eq!(MARKER_ENV_VAR, "FANDHE_BROWSER_JS_WORKER");
        assert_eq!(PROTOCOL_VERSION, 3);
    }

    /// JS-1: 想定される tag 値が予約分も含め重複しないこと（プロトコル
    /// 定義の回帰確認）。
    #[test]
    fn js_1_tag_values_are_distinct() {
        let tags = [
            tag::HELLO,
            tag::EVALUATE,
            tag::RESULT,
            tag::ERROR,
            tag::NATIVE_CALL,
            tag::NATIVE_RETURN,
            tag::SHUTDOWN,
            tag::REGISTER_GLOBAL_FUNCTION,
            tag::BIND_DOM_LIKE_OBJECT,
        ];
        let mut sorted = tags.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), tags.len(), "tag values must be distinct");
    }

    // ---- TASK-29.5a・Issue #524: ObjectHandle 値・bind フレーム ----

    fn sample_binding() -> DomLikeObjectBinding {
        DomLikeObjectBinding {
            handle: ObjectHandle::from_raw(7),
            name: "dom".to_string(),
            members: vec![
                DomLikeMember {
                    kind: DomLikeMemberKind::Method,
                    name: "setText".to_string(),
                    native_call_id: 100,
                },
                DomLikeMember {
                    kind: DomLikeMemberKind::Property,
                    name: "title".to_string(),
                    native_call_id: 101,
                },
            ],
        }
    }

    /// JS-1・TASK-29.5a: ObjectHandle 値のワイヤ表現が `[5, id LE 4B]`
    /// （5 バイト固定）で、`encoded_js_value_len` とも一致すること。
    #[test]
    fn js_1_object_handle_value_wire_format_is_tag5_plus_u32_le() {
        let value = JsValue::ObjectHandle(ObjectHandle::from_raw(0x0403_0201));
        let mut buf = Vec::new();
        encode_js_value(&value, &mut buf).expect("encode");
        assert_eq!(buf, vec![5, 1, 2, 3, 4]);
        assert_eq!(encoded_js_value_len(&value).expect("len"), 5);
    }

    /// JS-1・TASK-29.5a: 切り詰められた ObjectHandle 値は `Truncated`。
    #[test]
    fn js_1_decode_object_handle_rejects_truncated_id() {
        for len in 1..5 {
            let buf = [5u8, 1, 2, 3];
            assert!(matches!(
                decode_js_value(buf.get(..len.min(buf.len())).expect("slice")),
                Err(ProtocolError::Truncated)
            ));
        }
    }

    /// JS-1・TASK-29.5a: bind フレームの往復（Method/Property 混在・0 件）。
    #[test]
    fn js_1_bind_dom_like_object_round_trips() {
        let binding = sample_binding();
        let bytes = encode_bind_dom_like_object(&binding).expect("encode");
        assert_eq!(
            decode_bind_dom_like_object(&bytes).expect("decode"),
            binding
        );

        let empty = DomLikeObjectBinding {
            handle: ObjectHandle::from_raw(u32::MAX),
            name: "d".to_string(),
            members: Vec::new(),
        };
        let bytes = encode_bind_dom_like_object(&empty).expect("encode");
        assert_eq!(decode_bind_dom_like_object(&bytes).expect("decode"), empty);
    }

    /// JS-1・TASK-29.5a: 既知入力のバイト列を固定する（ワイヤ形式の回帰検出）。
    #[test]
    fn js_1_bind_dom_like_object_wire_format_is_fixed() {
        let binding = DomLikeObjectBinding {
            handle: ObjectHandle::from_raw(1),
            name: "d".to_string(),
            members: vec![DomLikeMember {
                kind: DomLikeMemberKind::Property,
                name: "ab".to_string(),
                native_call_id: 0x0201,
            }],
        };
        let expected: Vec<u8> = vec![
            1, 0, 0, 0, // handle
            1, 0, b'd', // name
            1, 0, 0, 0, // member_count
            1, // kind = Property
            1, 2, 0, 0, // native_call_id
            2, 0, b'a', b'b', // member name
        ];
        assert_eq!(
            encode_bind_dom_like_object(&binding).expect("encode"),
            expected
        );
    }

    /// JS-1・TASK-29.5a: decode の拒否経路（空・各位置の切り詰め・不正 UTF-8・
    /// 空の名前・長さ超過・未知の kind・重複・末尾の余分なバイト）。
    #[test]
    fn js_1_decode_bind_dom_like_object_rejects_invalid_payloads() {
        let valid = encode_bind_dom_like_object(&sample_binding()).expect("encode");
        assert!(matches!(
            decode_bind_dom_like_object(&[]),
            Err(ProtocolError::Truncated)
        ));
        for cut in 0..valid.len() {
            let head = valid.get(..cut).expect("slice");
            assert!(
                decode_bind_dom_like_object(head).is_err(),
                "truncation at {cut} must be rejected"
            );
        }

        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(matches!(
            decode_bind_dom_like_object(&trailing),
            Err(ProtocolError::TrailingBytes { .. })
        ));

        // グローバル名（offset 6）を不正 UTF-8 にする。
        let mut bad_global = valid.clone();
        *bad_global.get_mut(6).expect("byte") = 0xff;
        assert!(matches!(
            decode_bind_dom_like_object(&bad_global),
            Err(ProtocolError::InvalidUtf8)
        ));

        // 先頭メンバーの名前先頭バイトを不正 UTF-8 にする
        // （4+2+3+4 + kind1+id4+len2 = 20）。
        let mut bad_member = valid.clone();
        *bad_member.get_mut(20).expect("byte") = 0xff;
        assert!(matches!(
            decode_bind_dom_like_object(&bad_member),
            Err(ProtocolError::InvalidUtf8)
        ));

        // 空のグローバル名。
        let mut empty_name = vec![0, 0, 0, 0, 0, 0];
        empty_name.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            decode_bind_dom_like_object(&empty_name),
            Err(ProtocolError::EmptyDomLikeName)
        ));

        // 長すぎるグローバル名（257 B）。
        let mut long_name = vec![0, 0, 0, 0];
        long_name.extend_from_slice(&257u16.to_le_bytes());
        long_name.extend(std::iter::repeat_n(b'a', 257));
        long_name.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            decode_bind_dom_like_object(&long_name),
            Err(ProtocolError::DomLikeNameTooLong { len: 257, max: 256 })
        ));

        // 未知の kind（2・255）。kind は offset 13。
        for kind in [2u8, 255] {
            let mut bad_kind = valid.clone();
            *bad_kind.get_mut(13).expect("byte") = kind;
            assert!(matches!(
                decode_bind_dom_like_object(&bad_kind),
                Err(ProtocolError::InvalidDomLikeMemberKind(k)) if k == kind
            ));
        }

        // メンバー名の重複。
        let mut dup_bytes = Vec::new();
        dup_bytes.extend_from_slice(&7u32.to_le_bytes());
        dup_bytes.extend_from_slice(&[3, 0]);
        dup_bytes.extend_from_slice(b"dom");
        dup_bytes.extend_from_slice(&2u32.to_le_bytes());
        for id in [100u32, 101] {
            dup_bytes.push(0);
            dup_bytes.extend_from_slice(&id.to_le_bytes());
            dup_bytes.extend_from_slice(&[1, 0, b'x']);
        }
        assert!(matches!(
            decode_bind_dom_like_object(&dup_bytes),
            Err(ProtocolError::DuplicateDomLikeMemberName)
        ));
    }

    /// JS-1・TASK-29.5a: 巨大な `member_count` を主張するフレームは確保前に
    /// `TooManyDomLikeMembers` で拒否されること。
    #[test]
    fn js_1_decode_bind_dom_like_object_rejects_huge_member_count_before_allocating() {
        let mut payload = vec![0, 0, 0, 0, 1, 0, b'd'];
        payload.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode_bind_dom_like_object(&payload),
            Err(ProtocolError::TooManyDomLikeMembers { max: 256, .. })
        ));
    }

    /// JS-1・TASK-29.5a: encode 側の拒否経路（件数超過・名前長超過・空の名前・
    /// メンバー名の重複）。
    #[test]
    fn js_1_encode_bind_dom_like_object_rejects_invalid_bindings() {
        let member = |name: &str| DomLikeMember {
            kind: DomLikeMemberKind::Method,
            name: name.to_string(),
            native_call_id: 1,
        };
        let base = |name: &str, members: Vec<DomLikeMember>| DomLikeObjectBinding {
            handle: ObjectHandle::from_raw(1),
            name: name.to_string(),
            members,
        };

        let too_many: Vec<DomLikeMember> = (0..=MAX_DOM_LIKE_OBJECT_MEMBERS)
            .map(|i| member(&format!("m{i}")))
            .collect();
        assert!(matches!(
            encode_bind_dom_like_object(&base("d", too_many)),
            Err(ProtocolError::TooManyDomLikeMembers {
                count: 257,
                max: 256
            })
        ));
        assert!(matches!(
            encode_bind_dom_like_object(&base(&"a".repeat(257), vec![])),
            Err(ProtocolError::DomLikeNameTooLong { len: 257, max: 256 })
        ));
        assert!(matches!(
            encode_bind_dom_like_object(&base("", vec![])),
            Err(ProtocolError::EmptyDomLikeName)
        ));
        assert!(matches!(
            encode_bind_dom_like_object(&base("d", vec![member("")])),
            Err(ProtocolError::EmptyDomLikeName)
        ));
        assert!(matches!(
            encode_bind_dom_like_object(&base("d", vec![member(&"a".repeat(257))])),
            Err(ProtocolError::DomLikeNameTooLong { len: 257, max: 256 })
        ));
        assert!(matches!(
            encode_bind_dom_like_object(&base("d", vec![member("x"), member("x")])),
            Err(ProtocolError::DuplicateDomLikeMemberName)
        ));
    }

    /// JS-1・TASK-29.5a: 上限いっぱい（256 メンバー・256 B の名前）のフレームが
    /// 往復し、長さが 67,594 B かつ親 → 子の上限以下であること。
    #[test]
    fn js_1_bind_dom_like_object_at_limits_round_trips_within_frame_limit() {
        let members: Vec<DomLikeMember> = (0..MAX_DOM_LIKE_OBJECT_MEMBERS)
            .map(|i| {
                let suffix = format!("{i:03}");
                let mut name = "a".repeat(MAX_DOM_LIKE_MEMBER_NAME_BYTES - suffix.len());
                name.push_str(&suffix);
                DomLikeMember {
                    kind: DomLikeMemberKind::Method,
                    name,
                    native_call_id: u32::try_from(i).expect("fits"),
                }
            })
            .collect();
        let binding = DomLikeObjectBinding {
            handle: ObjectHandle::from_raw(1),
            name: "g".repeat(MAX_GLOBAL_FUNCTION_NAME_BYTES),
            members,
        };
        let bytes = encode_bind_dom_like_object(&binding).expect("encode");
        assert_eq!(bytes.len(), 67_594);
        assert!(bytes.len() <= MAX_FRAME_PAYLOAD_PARENT_TO_CHILD);
        assert_eq!(
            decode_bind_dom_like_object(&bytes).expect("decode"),
            binding
        );
    }

    /// JS-1・TASK-29.5a: bind フレームの上限定数が doc どおりの具体値であること。
    #[test]
    fn js_1_dom_like_limits_have_the_documented_values() {
        assert_eq!(MAX_DOM_LIKE_OBJECT_MEMBERS, 256);
        assert_eq!(MAX_DOM_LIKE_MEMBER_NAME_BYTES, 256);
        assert_eq!(tag::BIND_DOM_LIKE_OBJECT, 9);
    }
}
