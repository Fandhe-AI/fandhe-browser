//! JS 評価を行う子プロセスの入口（`JS-1`・`TASK-29`・Issue #503「JS
//! プロセス分離」設計書 §3.1・§3.2・§3.5・§7 W3）。
//!
//! 呼び出し元: [`super::dispatch_worker`]（`lib.rs`）が、環境変数
//! [`super::worker_protocol::MARKER_ENV_VAR`]（`FANDHE_BROWSER_JS_WORKER`）
//! が設定されている場合に [`worker_main`] を呼ぶ。値はプロトコル
//! バージョン（[`super::worker_protocol::PROTOCOL_VERSION`]）の文字列
//! 表現であることを期待し、一致しなければ Hello を送らずに失敗終了する
//! （設計書 §3.1「ハンドシェイク」）。
//!
//! 子プロセスは [`super::v8_engine::V8Engine`] を 1 つだけ生成し、
//! プロセスの寿命のあいだ永続 Context を保ち続ける（設計書 §3.2）。
//! stdin から [`super::worker_protocol`] のフレームを読み、評価結果・
//! エラーを stdout へフレームとして書き返す。**stdout はプロトコル専用**
//! であり、本モジュールは `println!`/`print!` を使わない。stderr は
//! 診断用（V8 の fatal メッセージもここに出る）であり、親
//! （[`super::process_engine`]。W4）が末尾を読み取って
//! `ResourceLimitExceeded` の判定に使う。
//!
//! # プロセス全体としての挙動（実装済みを装わない。REPAIR-3）
//!
//! - ヒープ上限（[`super::v8_engine`] の `MAX_ISOLATE_HEAP_BYTES`）に
//!   達すると、本モジュールの制御が及ばないところで V8 の既定の
//!   fatal OOM によりこのプロセス自体が終了する（`evaluate_script` の
//!   呼び出しから制御が戻らない）。この場合、親は stdout の EOF と
//!   stderr の内容から `ResourceLimitExceeded` を推定する
//!   （`super::process_engine` のドキュメントコメント参照）
//! - 本モジュールは孤児プロセス化を防ぐため、stdin を読み続けることで
//!   親の生死を検出する。親が終了すれば stdin が EOF になり、本モジュール
//!   は正常終了する（設計書 §3.2「孤児の防止」）。**孤児の防止には**
//!   Job Object・PDEATHSIG のような OS 固有の仕組みには依存しない（下記
//!   のとおり Job Object 自体はメモリ上限の目的で Windows でのみ使う）
//! - ヒープ外メモリ（`ArrayBuffer` の backing store 等）にも
//!   [`super::resource_limits::enforce_child_memory_limit`] が起動直後・
//!   V8 初期化前に OS 側の上限を設定する（Linux の `RLIMIT_DATA`・
//!   Windows の Job Object working set 上限。設定に失敗したら評価を
//!   始めずに終了する）。OS ごとの強制の強さ・既知の制限は
//!   `super::resource_limits` のドキュメントコメントを参照
//! - Linux 限定でさらに、`enforce_child_memory_limit` より前に
//!   [`super::resource_limits::prefer_child_as_oom_victim`] を呼び、
//!   `/proc/self/oom_score_adj` を最大値へ設定する（`JS-1`・`TASK-29`・
//!   Issue #516）。これは確保量の上限ではなく、システム全体が
//!   メモリ逼迫した際に OOM killer が親より先にこの子を殺すようにする
//!   優先度のヒントである。失敗しても fail-closed にはせず、stderr へ
//!   警告を出して評価の準備を続行する（既知の制限として記録する。
//!   procfs が使えない環境要因に限られ、他の防御には影響しない）
//! - 本プロセスはセキュリティ上のサンドボックスではない。親と同じ
//!   ユーザー権限で動作し、seccomp 等の権限制限も行わない。得られるのは
//!   クラッシュ・メモリの資源分離だけである（`super::process_engine` の
//!   ドキュメントコメント参照）
//! - V8 の Platform は protected 版（thread-isolated allocation 有効）で
//!   初期化する（`JS-1`・`TASK-29`・Issue #520。[`worker_main`] 内、
//!   `enforce_child_memory_limit()` の後・`V8Engine::new*` の前で
//!   [`super::v8_engine::ensure_v8_initialized_with`] を呼ぶ）。本
//!   プロセスは単一スレッドで初期化から Isolate 生成まで進むため、
//!   protected 版が要求する「Isolate に入るのは `V8::initialize()` を
//!   呼んだスレッドの子孫だけ」という制約に抵触しない。子プロセスは
//!   親と同じユーザー権限で動くため効果は限定的だが、V8 内部のメモリ
//!   破壊に対する多層防御が 1 つ増える（同 Issue のドキュメントコメント
//!   参照。テストプロセス内の経路は引き続き unprotected のままにする
//!   理由は [`super::v8_engine::ensure_v8_initialized_with`] のドキュメント
//!   コメント参照）

use std::io::{self, Read, Write};
use std::process::ExitCode;

use super::engine_trait::{EngineKind, EvaluateOptions, JsEngineError, JsValue};
use super::shared::{NativeCallFailure, NativeCallTransport};
#[cfg(feature = "js-v8")]
use super::v8_engine::V8Engine;
use super::worker_protocol::{self, ErrorKind, NativeReturn, ProtocolError, tag};

/// 親が子プロセスへ「どのエンジンで評価するか」を伝える環境変数
/// （`TASK-32.2`・Issue #166）。値は [`EngineKind::as_str`] の表記
/// （`"v8"`/`"boa"`）。親（[`super::process_engine`]）は `env_clear()` した
/// うえで必ずこの値を渡す。未設定は V8 として扱う（親を介さない直接起動の
/// 後方互換。TEST 用途）。値は untrusted な入力として厳密に照合し、
/// 未知の値は fail-closed で起動を拒否する。
pub(crate) const ENGINE_ENV_VAR: &str = "FANDHE_BROWSER_JS_WORKER_ENGINE";

/// [`ENGINE_ENV_VAR`] の生の値から [`EngineKind`] を決める（未設定は V8）。
/// 未知の値は `Err`（成功を装わない）。
fn engine_kind_from_env_value(raw: Option<&str>) -> Result<EngineKind, String> {
    match raw {
        None => Ok(EngineKind::V8),
        Some("v8") => Ok(EngineKind::V8),
        Some("boa") => Ok(EngineKind::Boa),
        Some(other) => Err(format!(
            "unknown worker engine {:?} in {ENGINE_ENV_VAR}",
            other.chars().take(32).collect::<String>()
        )),
    }
}

/// 子プロセスのフレームループが評価エンジンへ要求する最小の窓口
/// （`TASK-32.2`・Issue #166）。V8（[`V8Engine`]）と boa
/// （[`super::boa_worker::BoaChildEngine`]）の両方が実装し、
/// [`worker_main`] 以降の処理をエンジン非依存にする。
pub(crate) trait ChildEngine {
    /// スクリプトを永続 Context で評価する。
    fn evaluate_script(
        &mut self,
        script: &str,
        options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError>;

    /// 逆方向 RPC が fatal で終わっていれば、そのメッセージを取り出す
    /// （取り出すと `None` に戻る）。
    fn take_native_call_fatal(&mut self) -> Option<String>;

    /// 親側関数 `id` へ転送するグローバル関数 `name` を登録する。
    fn install_native_proxy_global(&mut self, name: &str, id: u32) -> Result<(), JsEngineError>;

    /// DOM 風オブジェクトを登録する。
    fn install_dom_like_object(
        &mut self,
        binding: &worker_protocol::DomLikeObjectBinding,
    ) -> Result<(), JsEngineError>;
}

#[cfg(feature = "js-v8")]
impl ChildEngine for V8Engine {
    fn evaluate_script(
        &mut self,
        script: &str,
        options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        V8Engine::evaluate_script(self, script, options)
    }

    fn take_native_call_fatal(&mut self) -> Option<String> {
        V8Engine::take_native_call_fatal(self)
    }

    fn install_native_proxy_global(&mut self, name: &str, id: u32) -> Result<(), JsEngineError> {
        V8Engine::install_native_proxy_global(self, name, id)
    }

    fn install_dom_like_object(
        &mut self,
        binding: &worker_protocol::DomLikeObjectBinding,
    ) -> Result<(), JsEngineError> {
        V8Engine::install_dom_like_object(self, binding)
    }
}

/// テスト専用: 子プロセスの V8 Isolate に設定するヒープ上限（バイト）を
/// 上書きする環境変数（Issue #503 設計書 §7 W6「テスト用に小さいヒープ
/// 上限を渡す経路（テスト専用。本番では無効）」）。
///
/// [`super::worker_protocol::MARKER_ENV_VAR`] と併用した場合のみ効果を
/// 持つ。本番の起動経路（[`super::process_engine`]。W4）は子プロセスを
/// `env_clear()` した状態で起動し、この環境変数を明示的に渡さない限り
/// 子プロセス側には存在しないため、本番では常に既定のヒープ上限
/// （[`V8Engine::new`]）が使われる。値が未設定または `usize` として
/// 解釈できない場合も既定値を使う。
///
/// この環境変数の値は untrusted な入力として扱う（coding-rust.md
/// 「外部入力」節）: 親（`super::process_engine`）を経由せずこの
/// プロセスを直接起動し、既定値（[`super::v8_engine::MAX_ISOLATE_HEAP_BYTES`]）
/// を超える値を指定することで本番の上限を回避しようとする経路を防ぐため、
/// [`super::v8_engine::clamp_test_heap_limit_bytes`] で読み取り直後に
/// クランプする（codex レビュー指摘 #503 P0 対応。親側のクランプ
/// （`super::process_engine::WorkerSpawnConfigForTest::heap_limit_bytes`。
/// 型自体は feature `test-support` の有無に関わらず常に存在するが、
/// `test-support` 無効時は非公開 `use` により process_engine モジュール外
/// （本モジュールを含む他モジュール）から不可視になるためリンクにできない。
/// Issue #528）と合わせた多層防御であり、どちらか一方が壊れても上限は
/// 保たれる）。
#[cfg(feature = "js-v8")]
pub(crate) const TEST_HEAP_LIMIT_ENV_VAR: &str = "FANDHE_BROWSER_JS_WORKER_HEAP_LIMIT_BYTES";

/// テスト専用: 子プロセスが送る `Hello` フレームのエンジン種別を
/// 上書きする環境変数（codex レビュー指摘 #503 P1「Hello のエンジン
/// 種別を無視している」の回帰テストが、実際の子プロセスに
/// `EngineKind::Boa` を名乗らせて親側の拒否を確認するために使う）。
/// 値が `"boa"` のときだけ `EngineKind::Boa` を使い、それ以外（未設定を
/// 含む）は常に本番と同じ `EngineKind::V8` を送る。
///
/// [`TEST_HEAP_LIMIT_ENV_VAR`] と同様、本番の起動経路
/// （[`super::process_engine`]）は `env_clear()` した状態で子プロセスを
/// 起動するため、この環境変数を明示的に渡さない限り本番では常に
/// `EngineKind::V8` のままである。
pub(crate) const TEST_HELLO_ENGINE_OVERRIDE_ENV_VAR: &str =
    "FANDHE_BROWSER_JS_WORKER_HELLO_ENGINE_OVERRIDE";

/// テスト専用: 子プロセスが送る `Hello` フレームの末尾に、余分な 1
/// バイトを付け足す環境変数（codex レビュー指摘 #503 P1「decode_hello が
/// 末尾の余分なバイトを拒否していない」の回帰テストが、実際の子プロセス
/// に長さの違う `Hello` を送らせて親側の拒否を確認するために使う）。値が
/// 存在すれば（内容は問わない）付け足し、未設定なら本番と同じ 3 バイト
/// ちょうどの `Hello` を送る。
pub(crate) const TEST_HELLO_EXTRA_BYTE_ENV_VAR: &str = "FANDHE_BROWSER_JS_WORKER_HELLO_EXTRA_BYTE";

/// テスト専用: 子プロセスが起動直後に事前登録すべき逆方向 RPC プロキシの
/// 一覧を渡す環境変数（`JS-1`・`TASK-29`・Issue #526）。
///
/// 値は `name=id` を `,` で区切った形式（例: `f=1,g=2`）。子側
/// （[`worker_main`]）はこの値を **untrusted な外部入力**として扱い、
/// 全体長・件数の上限を検査してから解析する（各要素の `name` の妥当性は
/// [`super::v8_engine::V8Engine::install_native_proxy_global`] 自身が
/// 検証する）。
///
/// 本番の登録は親→子の登録フレーム（`REGISTER_GLOBAL_FUNCTION`。
/// `TASK-29.4`・Issue #155）で行う。この環境変数は「親側に `NativeFn` が
/// 無い未登録 id のプロキシ」を作るテスト専用の経路で、
/// **`test-support` feature でのみ**読む（本番の起動経路
/// （`super::process_engine::V8ProcessEngine::spawn_worker`）は
/// `env_clear()` した状態で子を起動するため、`test-support` が有効な
/// ビルドでも本番の `new()` 経由では値が渡らない。テストのみが
/// `WorkerSpawnConfigForTest::native_proxies_for_test` 経由で設定する）。
/// 値は `WorkerSpawnConfigForTest::native_proxies_for_test` から、子を
/// 起動するたびに組み立て直される（ホスト登録簿の分は登録フレームで
/// 登録し直すため、この環境変数には載せない）。
#[cfg(feature = "test-support")]
pub(crate) const TEST_NATIVE_PROXIES_ENV_VAR: &str = "FANDHE_BROWSER_JS_WORKER_TEST_NATIVE_PROXIES";

/// [`TEST_NATIVE_PROXIES_ENV_VAR`] に許す最大の値の長さ（バイト）。
/// DoS 対策の上限（外部入力を解析する前に検査する。coding-rust.md「外部
/// 入力」節）。
///
/// 親（`super::process_engine`）が環境変数の値を組み立てる際にも同じ上限を
/// 使うため、feature に関わらず存在させる（`TASK-29`・Issue #527）。
pub(crate) const MAX_TEST_NATIVE_PROXIES_ENV_VAR_BYTES: usize = 4096;

/// [`TEST_NATIVE_PROXIES_ENV_VAR`] に許す最大の登録件数。
///
/// 親が値を組み立てる際にも同じ上限を使うため、feature に関わらず存在
/// させる（`TASK-29`・Issue #527）。
pub(crate) const MAX_TEST_NATIVE_PROXIES: usize = 16;

/// [`TEST_NATIVE_PROXIES_ENV_VAR`] の生の値を `(name, id)` の列へ解析する
/// （`JS-1`・`TASK-29`・Issue #526）。
///
/// 外部入力として扱う: 全体長・件数を確保の前に検証し、`id` は `u32` として
/// 解析できることだけを確認する（`name` 自体の妥当性・重複チェックは
/// 呼び出し元が `install_native_proxy_global` を呼んだ結果の `Err` に
/// 委ねる。二重に検証しない）。
///
/// 値が未設定・空文字列の場合は空の `Vec`（登録なし）を返す。形式が
/// 不正な場合は `Err` を返し、呼び出し元（[`worker_main`]）はハンドシェイク
/// を送らずに失敗終了する（fail-closed）。
#[cfg(feature = "test-support")]
fn parse_test_native_proxies_env_value(raw: Option<&str>) -> Result<Vec<(String, u32)>, String> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if raw.len() > MAX_TEST_NATIVE_PROXIES_ENV_VAR_BYTES {
        return Err(format!(
            "{TEST_NATIVE_PROXIES_ENV_VAR} exceeds the maximum supported length of \
             {MAX_TEST_NATIVE_PROXIES_ENV_VAR_BYTES} bytes"
        ));
    }
    let mut proxies = Vec::new();
    for entry in raw.split(',') {
        if proxies.len() >= MAX_TEST_NATIVE_PROXIES {
            return Err(format!(
                "{TEST_NATIVE_PROXIES_ENV_VAR} has more than {MAX_TEST_NATIVE_PROXIES} entries"
            ));
        }
        let Some((name, id_str)) = entry.split_once('=') else {
            return Err(format!(
                "{TEST_NATIVE_PROXIES_ENV_VAR} entry {entry:?} is not in name=id form"
            ));
        };
        let id: u32 = id_str.trim().parse().map_err(|_| {
            format!("{TEST_NATIVE_PROXIES_ENV_VAR} entry {entry:?} has a non-u32 id")
        })?;
        proxies.push((name.to_string(), id));
    }
    Ok(proxies)
}

/// テスト専用（Windows のみ）: 子プロセスの Job Object
/// `ProcessMemoryLimit` を本番値（384 MiB）より**下げる**環境変数
/// （`JS-1`・`TASK-29`・Issue #531。親側の RSS 監視が先に発動しない構成で、
/// OS が確保を拒否する経路を検証するため）。
///
/// [`TEST_HEAP_LIMIT_ENV_VAR`] と同様、本番の起動経路は `env_clear()` した
/// 状態で子を起動するため、明示的に渡さない限り本番では存在しない。値は
/// untrusted な入力として扱い、子側で
/// [`super::resource_limits::clamp_test_windows_process_memory_limit_bytes`]
/// により本番値以下へクランプする（親側のクランプとは独立した多層防御）。
#[cfg(target_os = "windows")]
pub(crate) const TEST_WINDOWS_PROCESS_MEMORY_LIMIT_ENV_VAR: &str =
    "FANDHE_BROWSER_JS_WORKER_WINDOWS_PROCESS_MEMORY_LIMIT_BYTES";

/// 環境変数のバイト数文字列を `u64` で解釈し、`usize` に収まらない値は
/// `usize::MAX` へ飽和させる（32 ビット環境でも `usize` 範囲外の巨大値が
/// 解釈失敗（`None` = 既定値）にならず、後段のクランプで上限へ丸められる。
/// 3 OS 対応・`JS-1`・`TASK-29`）。数値として解釈できなければ `None`。
#[cfg(any(feature = "js-v8", target_os = "windows"))]
fn parse_env_bytes_saturating(value: &str) -> Option<usize> {
    let parsed = value.trim().parse::<u64>().ok()?;
    Some(usize::try_from(parsed).unwrap_or(usize::MAX))
}

/// [`TEST_WINDOWS_PROCESS_MEMORY_LIMIT_ENV_VAR`] の生の値から、
/// `enforce_child_memory_limit` へ渡す上書き値を決める純粋関数。解釈
/// できなければ `None`（本番値）、解釈できればクランプした値を返す。
#[cfg(target_os = "windows")]
fn test_windows_process_memory_limit_from_env_value(raw: Option<&str>) -> Option<usize> {
    raw.and_then(parse_env_bytes_saturating)
        .map(super::resource_limits::clamp_test_windows_process_memory_limit_bytes)
}

/// [`TEST_HEAP_LIMIT_ENV_VAR`] の生の値（未設定なら `None`）から、実際に
/// [`V8Engine::new_with_heap_limit`] へ渡すヒープ上限を決める（codex
/// レビュー指摘 #503 P0 対応）。
///
/// 純粋関数として切り出す理由: [`worker_main`] は環境変数・stdio に直接
/// 触れるため単体テストしづらい。パース・クランプだけを本関数に
/// 切り出すことで、子プロセスが受け取る値を untrusted な入力として扱う
/// クランプ処理（[`super::v8_engine::clamp_test_heap_limit_bytes`]）を、
/// 実プロセスを起動せずに具体値で検証できる（`tests` モジュール参照。
/// 親側のクランプ（`super::process_engine::spawn_worker`）とは独立した
/// 経路であり、親を経由しない直接起動に対する防御になっていることを
/// この関数単体のテストで確認する）。
///
/// - 値が無い、または `usize` として解釈できない場合は `None`（既定の
///   ヒープ上限を使う。[`V8Engine::new`]）
/// - 解釈できた場合は [`super::v8_engine::clamp_test_heap_limit_bytes`] で
///   クランプした値を返す（本番の既定値を超えられず、下限も満たす）
#[cfg(feature = "js-v8")]
fn test_heap_limit_from_env_value(raw: Option<&str>) -> Option<usize> {
    raw.and_then(parse_env_bytes_saturating)
        .map(super::v8_engine::clamp_test_heap_limit_bytes)
}

/// stdio 越しに親と一問一答する [`NativeCallTransport`] 実装（`JS-1`・
/// `TASK-29`・Issue #511）。
///
/// 呼び出し元: [`worker_main`] が `V8Engine::set_native_call_transport` で
/// 本番の子プロセスへ配線する。`R`/`W` をジェネリックにしているのは、
/// テストが実プロセス・実 stdio を介さず `Cursor<Vec<u8>>` で往復を検証
/// できるようにするため（`tests` モジュール参照）。
///
/// 呼び出し先で本番が使う実際のハンドル（[`worker_main`]）は、ループ本体と
/// 同じ `io::stdin()`/`io::stdout()`（呼び出しごとにロックする `Stdin`/
/// `Stdout`）である。`StdinLock`/`StdoutLock` を関数スコープで握り続けると、
/// 評価中に本トランスポートが同じ stdin/stdout を読み書きしようとした際に
/// 再入不能なロックで永久にブロックする（デッドロック）。[`worker_main`]
/// はこの理由でループ全体を通して `lock()` を保持しない（同関数の
/// ドキュメントコメント参照）。
pub(crate) struct StdioTransport<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> StdioTransport<R, W> {
    pub(crate) fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }
}

impl<R: Read, W: Write> NativeCallTransport for StdioTransport<R, W> {
    /// `NativeCall` を送り、`NativeReturn` が届くまでブロックする
    /// （一問一答）。
    ///
    /// - 送信前に件数・フレームサイズを検証し、上限超過は
    ///   [`NativeCallFailure::Rejected`]（非 fatal。JS へ `RangeError` を
    ///   投げるだけで、子・親のプロセス・接続は生き続ける）にする
    /// - それ以外（I/O エラー・EOF・想定外の tag・デコード失敗）は
    ///   [`NativeCallFailure::Fatal`] にする（一問一答の契約違反。
    ///   [`super::v8_engine::native_proxy_callback`] がこれを見て評価を
    ///   打ち切り、このプロセスを終了させる）
    fn call(&mut self, id: u32, args: &[JsValue]) -> Result<NativeReturn, NativeCallFailure> {
        let payload = match worker_protocol::encode_native_call(id, args) {
            Ok(payload) => payload,
            Err(err) => return Err(NativeCallFailure::Rejected(err.to_string())),
        };
        if payload.len() > worker_protocol::MAX_FRAME_PAYLOAD_CHILD_TO_PARENT {
            return Err(NativeCallFailure::Rejected(format!(
                "native call payload of {} bytes exceeds the {}-byte limit",
                payload.len(),
                worker_protocol::MAX_FRAME_PAYLOAD_CHILD_TO_PARENT
            )));
        }

        if let Err(err) = send_frame(&mut self.writer, tag::NATIVE_CALL, &payload) {
            return Err(NativeCallFailure::Fatal(format!(
                "failed to send NativeCall: {err}"
            )));
        }

        let frame = match worker_protocol::read_frame(
            &mut self.reader,
            worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
        ) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                return Err(NativeCallFailure::Fatal(
                    "parent closed the connection while awaiting a NativeReturn".to_string(),
                ));
            }
            Err(err) => {
                return Err(NativeCallFailure::Fatal(format!(
                    "protocol violation while awaiting a NativeReturn: {err}"
                )));
            }
        };
        let (frame_tag, payload) = frame;
        if frame_tag != tag::NATIVE_RETURN {
            return Err(NativeCallFailure::Fatal(format!(
                "expected a NativeReturn frame but received tag {frame_tag} \
                 (native call is a strict one-question-one-answer protocol)"
            )));
        }
        worker_protocol::decode_native_return(&payload)
            .map_err(|err| NativeCallFailure::Fatal(format!("invalid NativeReturn: {err}")))
    }
}

/// [`super::dispatch_worker`] から呼ばれる、子プロセスモードの本体
/// （`js-v8` feature 有効時）。
///
/// `marker_value`（[`super::worker_protocol::MARKER_ENV_VAR`] の値）を
/// プロトコルバージョンとして解釈する。パース失敗・バージョン不一致の
/// いずれも Hello を送らずに `ExitCode::FAILURE` で終了する
/// （設計書 §7 W6「ハンドシェイク失敗」のテスト経路。親側はハンドシェイク
/// タイムアウトまたは Hello 到達前の EOF として検出する）。
pub(crate) fn worker_main(marker_value: &str) -> ExitCode {
    let requested_version: u16 = match marker_value.trim().parse() {
        Ok(version) => version,
        Err(_) => {
            eprintln!(
                "fandhe-browser-js worker: {} value {marker_value:?} is not a valid protocol version",
                worker_protocol::MARKER_ENV_VAR
            );
            return ExitCode::FAILURE;
        }
    };
    if requested_version != worker_protocol::PROTOCOL_VERSION {
        eprintln!(
            "fandhe-browser-js worker: unsupported protocol version {requested_version} \
             (this binary implements version {})",
            worker_protocol::PROTOCOL_VERSION
        );
        return ExitCode::FAILURE;
    }

    // Linux 限定: V8 の初期化・メモリ上限設定より前に、自分自身を OOM
    // killer の優先対象にする（`JS-1`・`TASK-29`・Issue #516）。上限を
    // 課すものではなく、システム全体がメモリ逼迫した際に親（ホスト）
    // より先にこの子が殺される側になるようにするヒントに過ぎない
    // （`super::resource_limits` のドキュメントコメント「OS ごとの
    // 強制の強さ」節参照）。書き込みが失敗しても fail-closed にはせず、
    // 警告を出して評価の準備を続行する（procfs が使えない環境要因に
    // 限られ、他の防御には影響しないため。判断の理由は
    // `super::resource_limits::prefer_child_as_oom_victim` のドキュメント
    // コメント参照）。
    //
    // 警告メッセージは、親（`super::process_engine::WorkerHandle::
    // stderr_indicates_oom`）が `ResourceLimitExceeded` への分類に使う
    // 文字列（"Fatal JavaScript out of memory"・"Fatal process out of
    // memory"）を含めない（誤って OOM と分類されるのを防ぐため）。
    #[cfg(target_os = "linux")]
    if let Err(err) = super::resource_limits::prefer_child_as_oom_victim() {
        eprintln!(
            "fandhe-browser-js worker: warning: failed to set oom_score_adj \
             (continuing without OOM-killer preference): {err}"
        );
    }

    // V8 を初期化する（ひいては Isolate を生成する）前に、自分自身へ
    // OS のメモリ上限を設定する（codex レビュー指摘 #503 P0「ヒープ外
    // メモリが無制限」対応。`super::resource_limits` のドキュメント
    // コメント参照）。失敗したら評価を始めずに終了する（fail-closed）。
    //
    // 戻り値の `_memory_limit_guard` は `worker_main` 関数のスコープが
    // 終わるまで（＝子プロセスがこの後の評価ループを終えて終了する
    // まで）保持し続ける必要がある（codex レビュー指摘 #503 P1
    // 「Job をローカル変数のまま返しているためハンドルが閉じてしまう」
    // 対応。`ChildMemoryLimitGuard` のドキュメントコメント参照。`_` を
    // 先頭に付けた名前にすることで「未使用」の警告を避けつつ、値
    // そのものはこの関数の終わりまで drop されない）。
    //
    // Windows のテスト専用の上書き（Issue #531）は、環境変数を untrusted な
    // 入力として読み取り直後にクランプする（本番値を超えられない）。
    #[cfg(target_os = "windows")]
    let windows_limit_override = test_windows_process_memory_limit_from_env_value(
        std::env::var(TEST_WINDOWS_PROCESS_MEMORY_LIMIT_ENV_VAR)
            .ok()
            .as_deref(),
    );
    #[cfg(not(target_os = "windows"))]
    let windows_limit_override: Option<usize> = None;
    let _memory_limit_guard =
        match super::resource_limits::enforce_child_memory_limit(windows_limit_override) {
            Ok(guard) => guard,
            Err(err) => {
                eprintln!(
                    "fandhe-browser-js worker: failed to enforce the child memory limit: {err}"
                );
                return ExitCode::FAILURE;
            }
        };

    // エンジン種別は親が環境変数で明示する（`TASK-32.2`・Issue #166）。
    // untrusted な入力として厳密に照合し、未知の値・未同梱の種別は
    // 評価を始めずに終了する（fail-closed）。
    let engine_kind =
        match engine_kind_from_env_value(std::env::var(ENGINE_ENV_VAR).ok().as_deref()) {
            Ok(kind) => kind,
            Err(err) => {
                eprintln!("fandhe-browser-js worker: {err}");
                return ExitCode::FAILURE;
            }
        };
    let mut engine: Box<dyn ChildEngine> = match create_child_engine(engine_kind) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("fandhe-browser-js worker: {err}");
            return ExitCode::FAILURE;
        }
    };

    // JS-1・TASK-29・Issue #526: `test-support` ビルドに限り、結合テストが
    // 親側 `NativeCall` dispatch を実際の子プロセスで検証できるよう、
    // 起動直後（Hello を送る前）にプロキシを事前登録する。本番の登録は
    // 登録フレーム（`TASK-29.4`・#155）で、これは未登録 id を作る経路であり、
    // [`TEST_NATIVE_PROXIES_ENV_VAR`] のドキュメントコメントが述べる
    // とおり本番の起動経路には影響しない。
    #[cfg(feature = "test-support")]
    {
        let raw = std::env::var(TEST_NATIVE_PROXIES_ENV_VAR).ok();
        match parse_test_native_proxies_env_value(raw.as_deref()) {
            Ok(proxies) => {
                for (name, id) in proxies {
                    if let Err(err) = engine.install_native_proxy_global(&name, id) {
                        eprintln!(
                            "fandhe-browser-js worker: failed to install a test native proxy \
                             {name:?} (id {id}): {err}"
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            Err(err) => {
                eprintln!(
                    "fandhe-browser-js worker: invalid test native proxy configuration: {err}"
                );
                return ExitCode::FAILURE;
            }
        }
    }

    let mut stdout = io::stdout();

    // Hello は Isolate・永続 Context の生成に成功した後にだけ送る
    // （設計書 §3.1「Hello を送るのは V8Engine::new() が成功した後」。
    // 親から見て Hello の到達＝「子のエンジンが評価可能な状態になった」
    // ことを意味する）。
    //
    // `TEST_HELLO_ENGINE_OVERRIDE_ENV_VAR`・`TEST_HELLO_EXTRA_BYTE_ENV_VAR`
    // は codex レビュー指摘 #503 P1 の回帰テスト専用の分岐であり、
    // 本番の起動経路（`env_clear()` される）では常に未設定のため
    // 到達しない。
    let hello_engine = if std::env::var(TEST_HELLO_ENGINE_OVERRIDE_ENV_VAR).as_deref() == Ok("boa")
    {
        EngineKind::Boa
    } else {
        engine_kind
    };
    let mut hello_payload =
        worker_protocol::encode_hello(worker_protocol::PROTOCOL_VERSION, hello_engine);
    if std::env::var(TEST_HELLO_EXTRA_BYTE_ENV_VAR).is_ok() {
        hello_payload.push(0xff);
    }
    if let Err(err) = send_frame(&mut stdout, tag::HELLO, &hello_payload) {
        eprintln!("fandhe-browser-js worker: failed to send Hello: {err}");
        return ExitCode::FAILURE;
    }

    // JS-1・Issue #511: `StdinLock`/`StdoutLock` をこのループのスコープで
    // 保持し続けない（`StdioTransport` のドキュメントコメント参照）。
    // `io::stdin()` は呼び出しごとにロックする `Read` 実装であり、内部の
    // バッファは `Stdin` 自身（mutex の内側）に保持されるため、読み取り
    // 途中のデータが失われることはない。
    let mut stdin = io::stdin();
    // JS-1・TASK-29.4: 登録フレームで受け付けた件数（親を信頼せず子でも
    // 上限を検査する）。
    let mut registered_count: usize = 0;

    loop {
        let frame = match worker_protocol::read_frame(
            &mut stdin,
            worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
        ) {
            Ok(Some(frame)) => frame,
            // 親が stdin を閉じた（正常終了。設計書 §3.2「孤児の防止」）。
            Ok(None) => return ExitCode::SUCCESS,
            Err(err) => {
                eprintln!(
                    "fandhe-browser-js worker: protocol violation while reading a frame: {err}"
                );
                return ExitCode::FAILURE;
            }
        };
        let (frame_tag, payload) = frame;

        match frame_tag {
            tag::EVALUATE => {
                let script = match worker_protocol::decode_evaluate(&payload) {
                    Ok(script) => script,
                    Err(err) => {
                        eprintln!(
                            "fandhe-browser-js worker: protocol violation while decoding \
                             Evaluate: {err}"
                        );
                        return ExitCode::FAILURE;
                    }
                };
                match evaluate_and_respond(engine.as_mut(), &script, &mut stdout) {
                    Ok(RespondOutcome::Responded) => {}
                    Ok(RespondOutcome::NativeCallProtocolViolation(message)) => {
                        // JS-1・Issue #511: 逆方向 RPC が fatal
                        // （プロトコル違反・EOF・I/O エラー）で終わった。
                        // 応答フレームは送らない（`evaluate_and_respond` の
                        // ドキュメントコメント参照）。親はこれを EOF として
                        // 検出し `EngineUnavailable` へ変換する。
                        eprintln!(
                            "fandhe-browser-js worker: native call protocol violation: {message}"
                        );
                        return ExitCode::FAILURE;
                    }
                    Err(err) => {
                        eprintln!(
                            "fandhe-browser-js worker: failed to send a response frame: {err}"
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            tag::REGISTER_GLOBAL_FUNCTION => {
                match handle_register_global_function(
                    engine.as_mut(),
                    &payload,
                    &mut stdout,
                    &mut registered_count,
                ) {
                    Ok(()) => {}
                    Err(err) => {
                        // デコード失敗（プロトコル違反）と応答送信の I/O
                        // 失敗のどちらも、Context の状態を親と揃えられない
                        // ため終了する（親は EOF として検出する）。
                        eprintln!(
                            "fandhe-browser-js worker: failed to handle \
                             RegisterGlobalFunction: {err}"
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            tag::BIND_DOM_LIKE_OBJECT => {
                if let Err(err) = handle_bind_dom_like_object(
                    engine.as_mut(),
                    &payload,
                    &mut stdout,
                    &mut registered_count,
                ) {
                    // デコード失敗（プロトコル違反）と応答送信の I/O 失敗の
                    // どちらも Context の状態を親と揃えられないため終了する。
                    eprintln!(
                        "fandhe-browser-js worker: failed to handle BindDomLikeObject: {err}"
                    );
                    return ExitCode::FAILURE;
                }
            }
            tag::SHUTDOWN => return ExitCode::SUCCESS,
            other => {
                eprintln!("fandhe-browser-js worker: protocol violation: unexpected tag {other}");
                return ExitCode::FAILURE;
            }
        }
    }
}

/// 要求された種別の評価エンジンを子プロセスの中で生成し、逆方向 RPC の
/// transport（stdio）を配線して返す（`TASK-32.2`・Issue #166）。
///
/// 呼び出し元: [`worker_main`]。メモリ上限（`enforce_child_memory_limit`）を
/// 掛けた**後**に呼ぶ（OS の上限を先に掛ける順序を保つ）。feature で
/// 同梱されていない種別は `Err`（成功を装わない）。
fn create_child_engine(kind: EngineKind) -> Result<Box<dyn ChildEngine>, String> {
    match kind {
        EngineKind::V8 => create_v8_child_engine(),
        EngineKind::Boa => create_boa_child_engine(),
    }
}

#[cfg(feature = "js-v8")]
fn create_v8_child_engine() -> Result<Box<dyn ChildEngine>, String> {
    // V8 の Platform を protected 版（thread-isolated allocation 有効）で
    // 初期化する（`JS-1`・`TASK-29`・Issue #520）。この子プロセスは
    // `main`（`run_js_worker_if_requested` 経由）から本関数まで単一
    // スレッドで直線的に進み、Isolate に入るのもこのスレッドだけである
    // ため、`v8_engine::ensure_v8_initialized_with` のドキュメントコメント
    // が述べる「子孫スレッド制約」に抵触しない。呼ぶ位置は
    // `enforce_child_memory_limit()` の**後**・`V8Engine::new*` の**前**。
    //
    // プロトコルバージョン検証の早期 return より**必ず下**に置くこと
    // （`worker_main` を直接呼ぶユニットテストのプロセスが protected に
    // ならないようにするため。Issue #520 実装計画）。
    let actual_platform =
        super::v8_engine::ensure_v8_initialized_with(super::v8_engine::V8PlatformKind::Protected);
    if actual_platform != super::v8_engine::V8PlatformKind::Protected {
        // 正規の子プロセスでは起こり得ない。保護が有効であるかのように
        // 装わず、診断を出して評価は継続する（unprotected のままでも従来と
        // 同じ安全水準。security.md「偽装・回避機能の禁止」）。
        eprintln!(
            "fandhe-browser-js worker: V8 platform was already initialized as {actual_platform:?}; \
             thread-isolated allocation is not active"
        );
    }

    // 環境変数は untrusted な入力（`TEST_HEAP_LIMIT_ENV_VAR` のドキュメント
    // コメント参照）。読み取った直後にクランプする。
    let heap_limit_override =
        test_heap_limit_from_env_value(std::env::var(TEST_HEAP_LIMIT_ENV_VAR).ok().as_deref());
    let engine_result = match heap_limit_override {
        Some(bytes) => V8Engine::new_with_heap_limit(bytes),
        None => V8Engine::new(),
    };
    let mut engine =
        engine_result.map_err(|err| format!("failed to create the V8 engine: {err}"))?;

    // JS-1・Issue #511: 逆方向 RPC の transport を配線する（Hello を送る前）。
    // `io::stdin()`/`io::stdout()` は呼び出しごとにロックする（`StdioTransport`
    // のドキュメントコメント参照）。
    engine.set_native_call_transport(Box::new(StdioTransport::new(io::stdin(), io::stdout())));
    Ok(Box::new(engine))
}

#[cfg(not(feature = "js-v8"))]
fn create_v8_child_engine() -> Result<Box<dyn ChildEngine>, String> {
    Err("this binary was not built with the js-v8 feature".to_string())
}

/// macOS では boa を動かさない（fail-closed。親側の [`create_engine`] と同じ
/// 理由。`engine_trait::create_boa_engine` 参照）。親を経由しない直接起動も
/// 評価を始めずに終了させる。
///
/// [`create_engine`]: super::engine_trait::create_engine
#[cfg(all(feature = "js-boa", target_os = "macos"))]
fn create_boa_child_engine() -> Result<Box<dyn ChildEngine>, String> {
    Err("the boa engine is disabled on macOS because no allocation-time memory limit can be enforced"
        .to_string())
}

#[cfg(all(feature = "js-boa", not(target_os = "macos")))]
fn create_boa_child_engine() -> Result<Box<dyn ChildEngine>, String> {
    // boa 専用の追加メモリ上限（Linux のみ。他 OS は no-op）。
    // `enforce_child_memory_limit` の後に呼ぶため、上限は下げる方向にしか動かない。
    super::resource_limits::tighten_child_memory_limit_for_boa()?;
    // boa には Platform 初期化も専用のヒープ上限 API も無い。メモリは
    // 呼び出し元が先に掛けた OS 側の上限（`enforce_child_memory_limit`）と
    // 親の RSS 監視、時間は親の期限 kill で守る（AGENTS.md「リソース上限」）。
    Ok(Box::new(super::boa_worker::BoaChildEngine::new(Box::new(
        StdioTransport::new(io::stdin(), io::stdout()),
    ))))
}

#[cfg(not(feature = "js-boa"))]
fn create_boa_child_engine() -> Result<Box<dyn ChildEngine>, String> {
    Err("this binary was not built with the js-boa feature".to_string())
}

/// [`evaluate_and_respond`] の結果（`JS-1`・Issue #511）。
///
/// `Result<(), ProtocolError>` のままでは「応答フレームを送った（正常）」と
/// 「逆方向 RPC の fatal を検出したため応答フレームを送らなかった（異常。
/// プロセスを終了させる）」を呼び出し元が区別できないため、専用の enum に
/// する（coding-rust.md「戻り値は将来拡張できる構造を持つ型にする」）。
enum RespondOutcome {
    /// `Result`/`Error` フレームを送信した（通常経路）。
    Responded,
    /// 逆方向 RPC が fatal で終わったため、応答フレームを送らなかった
    /// （[`worker_main`] がこれを見てプロセスを終了させる）。
    NativeCallProtocolViolation(String),
}

/// 1 件の `Evaluate` を処理し、`Result`/`Error` フレームを書き返す
/// （`JS-1`・Issue #503・#511）。
///
/// 戻り値の `Err` はフレームの送信自体（I/O）が失敗したことを表す。
/// 評価そのものの失敗は `Error` フレームとして正常に送信し、
/// `Ok(RespondOutcome::Responded)` を返す（設計書 §3.2「タイムアウトでは
/// 子は死なず、Context もそのまま残る」。呼び出しループはこの後も継続
/// する）。
///
/// # 逆方向 RPC の fatal 確認（Issue #511）
///
/// 評価の直後・応答フレームを送信する**前**に
/// [`V8Engine::take_native_call_fatal`] を確認する。これを
/// `finish_watchdog`（`v8_engine.rs`）の判定より優先する理由: fatal 時は
/// `terminate_execution` を呼んでいるため `evaluate_script` の戻り値だけを
/// 見ると `has_terminated()` が真になり `JsEngineError::Timeout` に誤分類
/// されうる。`Some` の場合は `Result`/`Error` のどちらも送らず
/// `RespondOutcome::NativeCallProtocolViolation` を返す（呼び出し元が
/// プロセスを終了させる。親はこれを EOF として検出し `EngineUnavailable`
/// へ変換する。`super::process_engine` を参照）。
fn evaluate_and_respond(
    engine: &mut dyn ChildEngine,
    script: &str,
    stdout: &mut impl Write,
) -> Result<RespondOutcome, ProtocolError> {
    let evaluation = engine.evaluate_script(script, &EvaluateOptions::default());
    if let Some(fatal) = engine.take_native_call_fatal() {
        return Ok(RespondOutcome::NativeCallProtocolViolation(fatal));
    }
    match evaluation {
        Ok(value) => {
            let mut payload = Vec::new();
            worker_protocol::encode_js_value(&value, &mut payload)?;
            send_frame(stdout, tag::RESULT, &payload)?;
        }
        Err(err) => {
            let (kind, message) = classify_evaluation_error(&err);
            let payload = worker_protocol::encode_error(kind, &message);
            send_frame(stdout, tag::ERROR, &payload)?;
        }
    }
    Ok(RespondOutcome::Responded)
}

/// `REGISTER_GLOBAL_FUNCTION` フレーム 1 件を処理し、応答（成功は
/// `RESULT(Undefined)`、登録失敗は `ERROR{Binding}`）を書き返す
/// （`JS-1`・`TASK-29.4`・Issue #155）。
///
/// 呼び出し元: [`worker_main`] のフレームループ。親側の
/// `V8ProcessEngine::inject_global_function`（と起動時の登録し直し）が
/// 対のフレームを送る。`Binding` の失敗では子も Context も生きたまま
/// （`Err` を返さない）。`Err` はフレームの decode 失敗（プロトコル違反）
/// または応答送信の失敗のみで、呼び出し元がプロセスを終了させる。
fn handle_register_global_function(
    engine: &mut dyn ChildEngine,
    payload: &[u8],
    stdout: &mut impl Write,
    registered_count: &mut usize,
) -> Result<(), ProtocolError> {
    let (id, name) = worker_protocol::decode_register_global_function(payload)?;
    let outcome = if *registered_count >= worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS {
        Err(JsEngineError::BindingFailed(format!(
            "cannot register more than {} global functions",
            worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS
        )))
    } else {
        engine.install_native_proxy_global(&name, id)
    };
    respond_to_registration(outcome, stdout, registered_count)
}

/// `BIND_DOM_LIKE_OBJECT` フレーム 1 件を処理し、応答（成功は
/// `RESULT(Undefined)`、bind 失敗は `ERROR{Binding}`）を書き返す
/// （`JS-1`・`TASK-29.5b`・Issue #525）。
///
/// 呼び出し元: [`worker_main`] のフレームループ。親側の
/// `V8ProcessEngine::bind_dom_like_object`（と起動時の登録し直し）が対の
/// フレームを送る。DOM 風オブジェクト 1 個はグローバル枠 1 つとして
/// `registered_count` に数える（グローバル関数と同じ上限
/// [`worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS`]。親を信頼せず子でも
/// 上限をかける）。`Binding` 失敗では子も Context も生きたまま。
/// `Err` はデコード失敗（プロトコル違反）または応答送信の失敗のみ。
fn handle_bind_dom_like_object(
    engine: &mut dyn ChildEngine,
    payload: &[u8],
    stdout: &mut impl Write,
    registered_count: &mut usize,
) -> Result<(), ProtocolError> {
    let binding = worker_protocol::decode_bind_dom_like_object(payload)?;
    let outcome = if *registered_count >= worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS {
        Err(JsEngineError::BindingFailed(format!(
            "cannot register more than {} global names",
            worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS
        )))
    } else {
        engine.install_dom_like_object(&binding)
    };
    respond_to_registration(outcome, stdout, registered_count)
}

/// 登録系フレーム（関数注入・DOM 風 bind）の結果を応答フレームへ変換して
/// 書く共通部。成功なら件数を進めて `RESULT(Undefined)`、失敗は `ERROR`
/// （`Evaluation` 相当は防御的に `Binding` へ寄せる）。
fn respond_to_registration(
    outcome: Result<(), JsEngineError>,
    stdout: &mut impl Write,
    registered_count: &mut usize,
) -> Result<(), ProtocolError> {
    match outcome {
        Ok(()) => {
            *registered_count += 1;
            let mut out = Vec::new();
            worker_protocol::encode_js_value(&JsValue::Undefined, &mut out)?;
            send_frame(stdout, tag::RESULT, &out)
        }
        Err(err) => {
            let (kind, message) = classify_evaluation_error(&err);
            let kind = if kind == ErrorKind::Evaluation {
                // install 系は `BindingFailed` しか返さない契約だが、
                // 防御的に登録失敗として扱う。
                ErrorKind::Binding
            } else {
                kind
            };
            send_frame(
                stdout,
                tag::ERROR,
                &worker_protocol::encode_error(kind, &message),
            )
        }
    }
}

/// [`JsEngineError`] を [`ErrorKind`] と、[`worker_protocol::MAX_ERROR_MESSAGE_BYTES`]
/// までに切り詰めたメッセージへ変換する。
///
/// `JsEngineError` は `#[non_exhaustive]` だが、本 crate の内側からの
/// `match` は既知の全 variant を網羅すれば `_` 分岐は不要になる
/// （`#[non_exhaustive]` が制限するのは他 crate からの網羅性判定のみ）。
///
/// タイムアウトは [`JsEngineError::Timeout`] variant で直接判別する
/// （Issue #503 W5。以前はメッセージの文言（`"... timeout ..."`）で
/// 判別していたが、専用 variant を追加したため文言ヒューリスティックは
/// 不要になった）。
///
/// `JsEngineError::ResourceLimitExceeded` は `ErrorKind::ResourceLimit`
/// で送る（boa のループ・再帰・スタック上限。`TASK-32.2`・Issue #166）。
/// `JsEngineError::EngineUnavailable` はワイヤ上に専用の `ErrorKind` が無いため、
/// `Evaluation` に畳んで送る
/// （`EngineUnavailable` は `v8_engine` が NativeCall fatal 記録済みの
/// Context に対する評価要求で返しうる。親側では `EvaluationFailed` として
/// 見える。区別するにはプロトコル変更が必要で、`TASK-29.6.1` の範囲外）。
/// 親側の逆変換は `process_engine::error_frame_to_js_engine_error`、
/// V8 失敗からの全体対応表は `v8_engine.rs` のモジュール doc「エラー変換」
/// 節を参照。
fn classify_evaluation_error(err: &JsEngineError) -> (ErrorKind, String) {
    match err {
        JsEngineError::EvaluationFailed(msg) => (
            ErrorKind::Evaluation,
            worker_protocol::truncate_for_wire(msg),
        ),
        JsEngineError::BindingFailed(msg) => {
            (ErrorKind::Binding, worker_protocol::truncate_for_wire(msg))
        }
        JsEngineError::Timeout(msg) => {
            (ErrorKind::Timeout, worker_protocol::truncate_for_wire(msg))
        }
        JsEngineError::ResourceLimitExceeded(msg) => (
            ErrorKind::ResourceLimit,
            worker_protocol::truncate_for_wire(msg),
        ),
        JsEngineError::EngineUnavailable(msg) => (
            ErrorKind::Evaluation,
            worker_protocol::truncate_for_wire(msg),
        ),
    }
}

/// フレームを書き込んで即座に `flush` する。子・親どちらも stdio は
/// バッファリングされうるため、フレーム単位で確実に相手へ届ける
/// （設計書 §3.5）。
fn send_frame(writer: &mut impl Write, tag: u8, payload: &[u8]) -> Result<(), ProtocolError> {
    worker_protocol::write_frame(writer, tag, payload)?;
    writer.flush().map_err(ProtocolError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JS-1・TASK-29・Issue #526: 未設定・空文字列は登録なし（空の
    /// `Vec`）になること。
    #[cfg(feature = "test-support")]
    #[test]
    fn js_1_parse_test_native_proxies_env_value_treats_missing_or_empty_as_no_proxies() {
        assert_eq!(
            parse_test_native_proxies_env_value(None).expect("must parse"),
            Vec::new()
        );
        assert_eq!(
            parse_test_native_proxies_env_value(Some("")).expect("must parse"),
            Vec::new()
        );
    }

    /// JS-1・TASK-29・Issue #526: `name=id` を `,` で区切った正常な値が
    /// 順序どおりに解析されること。
    #[cfg(feature = "test-support")]
    #[test]
    fn js_1_parse_test_native_proxies_env_value_parses_valid_entries() {
        assert_eq!(
            parse_test_native_proxies_env_value(Some("f=1,g=2")).expect("must parse"),
            vec![("f".to_string(), 1), ("g".to_string(), 2)]
        );
    }

    /// JS-1・TASK-29・Issue #526: `=` が無い要素・`u32` として解釈できない
    /// `id` は `Err` になること（外部入力を検証してから使う）。
    #[cfg(feature = "test-support")]
    #[test]
    fn js_1_parse_test_native_proxies_env_value_rejects_malformed_entries() {
        assert!(parse_test_native_proxies_env_value(Some("no-equals-sign")).is_err());
        assert!(parse_test_native_proxies_env_value(Some("f=not-a-number")).is_err());
    }

    /// JS-1・TASK-29・Issue #526: 件数の上限を超える値は、確保前に `Err`
    /// になること（DoS 対策。coding-rust.md「外部入力」節）。
    #[cfg(feature = "test-support")]
    #[test]
    fn js_1_parse_test_native_proxies_env_value_rejects_too_many_entries() {
        let raw = (0..=MAX_TEST_NATIVE_PROXIES)
            .map(|i| format!("f{i}={i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_test_native_proxies_env_value(Some(&raw)).is_err());
    }

    /// JS-1・TASK-29・Issue #526: 全体長の上限を超える値は `Err` になること。
    #[cfg(feature = "test-support")]
    #[test]
    fn js_1_parse_test_native_proxies_env_value_rejects_oversized_value() {
        let raw = "f=".to_string() + &"1".repeat(MAX_TEST_NATIVE_PROXIES_ENV_VAR_BYTES + 1);
        assert!(parse_test_native_proxies_env_value(Some(&raw)).is_err());
    }

    /// codex レビュー指摘 #503 P0: 親を経由しない直接起動で
    /// [`TEST_HEAP_LIMIT_ENV_VAR`] に本番の既定値（128 MiB =
    /// 134,217,728 バイト）を大きく超える値を渡しても、子プロセス自身が
    /// 既定値へクランプすること（実プロセス・stdio を介さず、値の変換
    /// 経路だけを具体値で検証する）。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_test_heap_limit_from_env_value_clamps_an_oversized_direct_value() {
        // 4 GiB。本番の既定値（128 MiB）を大きく超える。
        assert_eq!(
            test_heap_limit_from_env_value(Some("4294967296")),
            Some(134_217_728)
        );
    }

    /// codex レビュー指摘 #503 P0: `0` を直接渡しても、実用上動作する
    /// 最小値（1 MiB = 1,048,576 バイト）まで引き上げられること。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_test_heap_limit_from_env_value_raises_zero_to_the_floor() {
        assert_eq!(test_heap_limit_from_env_value(Some("0")), Some(1_048_576));
    }

    /// codex レビュー指摘 #503 P0: `usize` として解釈できない値・値が
    /// 未設定の場合は `None`（既定のヒープ上限を使う経路）になること。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_test_heap_limit_from_env_value_falls_back_to_default_on_invalid_or_missing_input() {
        assert_eq!(test_heap_limit_from_env_value(Some("not-a-number")), None);
        assert_eq!(test_heap_limit_from_env_value(Some("")), None);
        assert_eq!(test_heap_limit_from_env_value(None), None);
    }

    /// Issue #531: Job Object 上限の環境変数値は、上限超過・非数値・未設定・
    /// 範囲内のいずれも具体値で期待どおりに変換されること。
    #[cfg(target_os = "windows")]
    #[test]
    fn js_1_test_windows_process_memory_limit_from_env_value_clamps_and_falls_back() {
        assert_eq!(
            test_windows_process_memory_limit_from_env_value(Some("4294967296")),
            Some(384 * 1024 * 1024)
        );
        assert_eq!(
            test_windows_process_memory_limit_from_env_value(Some("117440512")),
            Some(112 * 1024 * 1024)
        );
        assert_eq!(
            test_windows_process_memory_limit_from_env_value(Some("x")),
            None
        );
        assert_eq!(test_windows_process_memory_limit_from_env_value(None), None);
    }

    /// 3 OS 対応: `usize` に収まらない巨大値は解釈失敗にならず `usize::MAX`
    /// へ飽和し、数値でない値は `None` になること（32 ビット環境対応）。
    #[cfg(any(feature = "js-v8", target_os = "windows"))]
    #[test]
    fn js_1_parse_env_bytes_saturating_saturates_and_rejects_non_numeric() {
        assert_eq!(parse_env_bytes_saturating(" 1024 "), Some(1024));
        assert_eq!(
            parse_env_bytes_saturating("18446744073709551615"),
            Some(usize::MAX)
        );
        assert_eq!(parse_env_bytes_saturating("18446744073709551616"), None);
        assert_eq!(parse_env_bytes_saturating("x"), None);
    }

    /// JS-1・Issue #503 W5: `JsEngineError::Timeout` は `ErrorKind::Timeout`
    /// に、`EvaluationFailed`（構文エラー等）は `ErrorKind::Evaluation` に
    /// なること（専用 variant による判別。文言ヒューリスティックは
    /// 使わない）。
    #[test]
    fn js_1_classify_evaluation_error_distinguishes_timeout_from_other_evaluation_failures() {
        let (kind, _) = classify_evaluation_error(&JsEngineError::Timeout(
            "script execution exceeded the 2 second timeout and was terminated".to_string(),
        ));
        assert_eq!(kind, ErrorKind::Timeout);

        let (kind, _) = classify_evaluation_error(&JsEngineError::EvaluationFailed(
            "SyntaxError: Unexpected end of input".to_string(),
        ));
        assert_eq!(kind, ErrorKind::Evaluation);
    }

    /// JS-1・Issue #503: `BindingFailed`/`ResourceLimitExceeded`/
    /// `EngineUnavailable` も `match` が尽くしていること（コンパイル時の
    /// 網羅性チェックに加え、具体値でも確認する）。
    #[test]
    fn js_1_classify_evaluation_error_handles_binding_and_resource_limit_variants() {
        let (kind, message) =
            classify_evaluation_error(&JsEngineError::BindingFailed("nope".to_string()));
        assert_eq!(kind, ErrorKind::Binding);
        assert_eq!(message, "nope");

        let (kind, message) =
            classify_evaluation_error(&JsEngineError::ResourceLimitExceeded("oom".to_string()));
        assert_eq!(kind, ErrorKind::ResourceLimit);
        assert_eq!(message, "oom");

        let (kind, message) =
            classify_evaluation_error(&JsEngineError::EngineUnavailable("gone".to_string()));
        assert_eq!(kind, ErrorKind::Evaluation);
        assert_eq!(message, "gone");
    }

    /// JS-1・Issue #503: 未知のプロトコルバージョン文字列は Hello を
    /// 送らずに失敗終了すること（ハンドシェイク失敗の検証。設計書
    /// §7 W6）。実際のハンドシェイク（子プロセスの起動・stdout の読み
    /// 取り）は結合テスト（`tests/v8_worker.rs`。W6）で検証する。
    #[test]
    fn js_1_worker_main_rejects_unsupported_protocol_version() {
        let exit_code = worker_main("9999");
        assert_eq!(exit_code, ExitCode::FAILURE);
    }

    /// JS-1・Issue #503: プロトコルバージョンとして解釈できない文字列も
    /// 失敗終了すること。
    #[test]
    fn js_1_worker_main_rejects_non_numeric_protocol_version() {
        let exit_code = worker_main("not-a-version");
        assert_eq!(exit_code, ExitCode::FAILURE);
    }

    /// JS-1・Issue #511: `StdioTransport::call` が、期待どおりの
    /// `NativeCall` フレームを書き込み、事前に符号化した `NativeReturn` を
    /// 正しく読み取れること（実 stdio・実子プロセスを介さない往復確認）。
    #[test]
    fn js_1_stdio_transport_round_trips_a_native_call() {
        let mut response = Vec::new();
        worker_protocol::write_frame(
            &mut response,
            tag::NATIVE_RETURN,
            &worker_protocol::encode_native_return(&NativeReturn::Ok(JsValue::Number(42.0)))
                .expect("encode must succeed"),
        )
        .expect("write must succeed");

        let mut written = Vec::new();
        let mut transport = StdioTransport::new(io::Cursor::new(response), &mut written);
        let result = transport
            .call(7, &[JsValue::Number(1.0), JsValue::String("x".to_string())])
            .expect("call must succeed");
        assert_eq!(result, NativeReturn::Ok(JsValue::Number(42.0)));

        let mut expected = Vec::new();
        worker_protocol::write_frame(
            &mut expected,
            tag::NATIVE_CALL,
            &worker_protocol::encode_native_call(
                7,
                &[JsValue::Number(1.0), JsValue::String("x".to_string())],
            )
            .expect("encode must succeed"),
        )
        .expect("write must succeed");
        assert_eq!(written, expected);
    }

    /// JS-1・Issue #511: 親が読み取り側を閉じた（EOF）場合は `Fatal` を
    /// 返すこと。
    #[test]
    fn js_1_stdio_transport_call_is_fatal_on_eof() {
        let mut transport = StdioTransport::new(io::Cursor::new(Vec::<u8>::new()), Vec::new());
        assert!(matches!(
            transport.call(1, &[]),
            Err(NativeCallFailure::Fatal(_))
        ));
    }

    /// JS-1・Issue #511: `NATIVE_RETURN` 以外の想定外のタグは `Fatal` を
    /// 返すこと（一問一答の違反）。
    #[test]
    fn js_1_stdio_transport_call_is_fatal_on_unexpected_tag() {
        let mut response = Vec::new();
        worker_protocol::write_frame(&mut response, tag::EVALUATE, b"1 + 1")
            .expect("write must succeed");
        let mut transport = StdioTransport::new(io::Cursor::new(response), Vec::new());
        assert!(matches!(
            transport.call(1, &[]),
            Err(NativeCallFailure::Fatal(_))
        ));
    }

    /// JS-1・Issue #511: フレーム長が上限を超える応答は `Fatal` を返すこと。
    #[test]
    fn js_1_stdio_transport_call_is_fatal_on_oversized_frame() {
        let max = worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD;
        let mut response = Vec::new();
        let len_with_tag = u32::try_from(max + 2).expect("fits in u32");
        response.extend_from_slice(&len_with_tag.to_le_bytes());
        let mut transport = StdioTransport::new(io::Cursor::new(response), Vec::new());
        assert!(matches!(
            transport.call(1, &[]),
            Err(NativeCallFailure::Fatal(_))
        ));
    }

    /// JS-1・Issue #511: 引数の件数・合計サイズが上限を超える場合は
    /// `Rejected` になり、何も書き込まれないこと。
    #[test]
    fn js_1_stdio_transport_call_rejects_too_many_args_without_writing() {
        let mut written = Vec::new();
        let mut transport = StdioTransport::new(io::Cursor::new(Vec::<u8>::new()), &mut written);
        let args: Vec<JsValue> = (0..(worker_protocol::MAX_NATIVE_CALL_ARGS + 1))
            .map(|_| JsValue::Undefined)
            .collect();
        assert!(matches!(
            transport.call(1, &args),
            Err(NativeCallFailure::Rejected(_))
        ));
        assert!(written.is_empty(), "nothing must be written on rejection");
    }

    /// 登録フレーム処理の応答を 1 件読む補助（テスト用）。
    #[cfg(feature = "js-v8")]
    fn read_one_response(written: Vec<u8>) -> (u8, Vec<u8>) {
        worker_protocol::read_frame(&mut io::Cursor::new(written), 4096)
            .expect("frame must be readable")
            .expect("one frame must have been written")
    }

    /// JS-1・TASK-29.4: 登録に成功すると `RESULT(Undefined)` が書かれ、
    /// 件数が増えること。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_register_global_function_success_responds_with_undefined() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let mut written = Vec::new();
        let mut count = 0usize;
        let payload = worker_protocol::encode_register_global_function(3, "hostAdd");
        handle_register_global_function(&mut engine, &payload, &mut written, &mut count)
            .expect("registration must be handled");
        assert_eq!(count, 1);
        let (frame_tag, body) = read_one_response(written);
        assert_eq!(frame_tag, tag::RESULT);
        assert_eq!(
            worker_protocol::decode_js_value(&body).expect("decode"),
            (JsValue::Undefined, body.len())
        );
    }

    /// JS-1・TASK-29.4: 登録できない名前（`undefined` は non-configurable）
    /// は `ERROR{Binding}` になり、件数は増えず `Ok` を返す（子は生きる）。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_register_global_function_failure_responds_with_binding_error() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let mut written = Vec::new();
        let mut count = 0usize;
        let payload = worker_protocol::encode_register_global_function(0, "undefined");
        handle_register_global_function(&mut engine, &payload, &mut written, &mut count)
            .expect("a binding failure is not a protocol violation");
        assert_eq!(count, 0);
        let (frame_tag, body) = read_one_response(written);
        assert_eq!(frame_tag, tag::ERROR);
        assert_eq!(
            worker_protocol::decode_error(&body).expect("decode").0,
            ErrorKind::Binding
        );
    }

    /// JS-1・TASK-29.4: 件数が上限に達していれば `ERROR{Binding}` にする。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_register_global_function_rejects_beyond_the_count_limit() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let mut written = Vec::new();
        let mut count = worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS;
        let payload = worker_protocol::encode_register_global_function(0, "f");
        handle_register_global_function(&mut engine, &payload, &mut written, &mut count)
            .expect("handled");
        assert_eq!(count, worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS);
        let (frame_tag, _) = read_one_response(written);
        assert_eq!(frame_tag, tag::ERROR);
    }

    /// JS-1・TASK-29.4: 壊れたペイロードはプロトコル違反として `Err`。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_register_global_function_malformed_payload_is_an_error() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let mut written = Vec::new();
        let mut count = 0usize;
        assert!(
            handle_register_global_function(&mut engine, &[1, 2], &mut written, &mut count)
                .is_err()
        );
        assert!(written.is_empty());
    }

    #[cfg(feature = "js-v8")]
    fn sample_bind_payload(name: &str) -> Vec<u8> {
        use worker_protocol::{DomLikeMember, DomLikeMemberKind, DomLikeObjectBinding};
        worker_protocol::encode_bind_dom_like_object(&DomLikeObjectBinding {
            handle: crate::engine_trait::ObjectHandle::from_raw(0),
            name: name.to_string(),
            members: vec![DomLikeMember {
                kind: DomLikeMemberKind::Method,
                name: "m".to_string(),
                native_call_id: 1,
            }],
        })
        .expect("encode")
    }

    /// JS-1・TASK-29.5b: bind に成功すると `RESULT(Undefined)` が書かれ件数が増える。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_bind_dom_like_object_success_responds_with_undefined() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let mut written = Vec::new();
        let mut count = 0usize;
        handle_bind_dom_like_object(
            &mut engine,
            &sample_bind_payload("dom"),
            &mut written,
            &mut count,
        )
        .expect("handled");
        assert_eq!(count, 1);
        let (frame_tag, body) = read_one_response(written);
        assert_eq!(frame_tag, tag::RESULT);
        assert_eq!(
            worker_protocol::decode_js_value(&body).expect("decode"),
            (JsValue::Undefined, body.len())
        );
    }

    /// JS-1・TASK-29.5b: 登録できない名前は `ERROR{Binding}`（件数は増えず子は生きる）。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_bind_dom_like_object_failure_responds_with_binding_error() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let mut written = Vec::new();
        let mut count = 0usize;
        handle_bind_dom_like_object(
            &mut engine,
            &sample_bind_payload("undefined"),
            &mut written,
            &mut count,
        )
        .expect("a binding failure is not a protocol violation");
        assert_eq!(count, 0);
        let (frame_tag, body) = read_one_response(written);
        assert_eq!(frame_tag, tag::ERROR);
        assert_eq!(
            worker_protocol::decode_error(&body).expect("decode").0,
            ErrorKind::Binding
        );
    }

    /// JS-1・TASK-29.5b: 件数が上限に達していれば `ERROR{Binding}`。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_bind_dom_like_object_rejects_beyond_the_count_limit() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let mut written = Vec::new();
        let mut count = worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS;
        handle_bind_dom_like_object(
            &mut engine,
            &sample_bind_payload("dom"),
            &mut written,
            &mut count,
        )
        .expect("handled");
        assert_eq!(count, worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS);
        let (frame_tag, _) = read_one_response(written);
        assert_eq!(frame_tag, tag::ERROR);
    }

    /// JS-1・TASK-29.5b: 壊れたペイロードはプロトコル違反として `Err`。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_1_bind_dom_like_object_malformed_payload_is_an_error() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let mut written = Vec::new();
        let mut count = 0usize;
        assert!(
            handle_bind_dom_like_object(&mut engine, &[1, 2], &mut written, &mut count).is_err()
        );
        assert!(written.is_empty());
    }

    /// JS-1・TASK-29.6.1: 子側 [`JsEngineError`] がワイヤ（`ERROR` フレーム）を
    /// 経て親側で復元されるとき、variant とメッセージが期待どおりになること。
    #[test]
    fn js_1_error_round_trips_through_the_wire_to_the_parent_error() {
        use super::super::process_engine::error_frame_to_js_engine_error;

        let cases = [
            (
                JsEngineError::EvaluationFailed("SyntaxError: x".to_string()),
                JsEngineError::EvaluationFailed("SyntaxError: x".to_string()),
            ),
            (
                JsEngineError::BindingFailed("bind".to_string()),
                JsEngineError::BindingFailed("bind".to_string()),
            ),
            (
                JsEngineError::Timeout("script execution exceeded".to_string()),
                JsEngineError::Timeout(
                    "script execution timed out inside the JS worker process: \
                     script execution exceeded"
                        .to_string(),
                ),
            ),
            (
                JsEngineError::EngineUnavailable("gone".to_string()),
                JsEngineError::EvaluationFailed("gone".to_string()),
            ),
        ];
        for (child_error, expected_parent) in cases {
            let (kind, message) = classify_evaluation_error(&child_error);
            let body = worker_protocol::encode_error(kind, &message);
            let (decoded_kind, decoded_message) =
                worker_protocol::decode_error(&body).expect("decode");
            assert_eq!(
                format!(
                    "{:?}",
                    error_frame_to_js_engine_error(decoded_kind, decoded_message)
                ),
                format!("{expected_parent:?}")
            );
        }
    }
}
