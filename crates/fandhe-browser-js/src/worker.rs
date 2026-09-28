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

use std::io::{self, Write};
use std::process::ExitCode;

use super::engine_trait::{EngineKind, EvaluateOptions, JsEngineError};
use super::v8_engine::V8Engine;
use super::worker_protocol::{self, ErrorKind, ProtocolError, tag};

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
/// （[`super::process_engine::WorkerSpawnConfigForTest::heap_limit_bytes`]）
/// と合わせた多層防御であり、どちらか一方が壊れても上限は保たれる）。
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
fn test_heap_limit_from_env_value(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|value| value.trim().parse::<usize>().ok())
        .map(super::v8_engine::clamp_test_heap_limit_bytes)
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
    let _memory_limit_guard = match super::resource_limits::enforce_child_memory_limit() {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("fandhe-browser-js worker: failed to enforce the child memory limit: {err}");
            return ExitCode::FAILURE;
        }
    };

    // V8 の Platform を protected 版（thread-isolated allocation 有効）で
    // 初期化する（`JS-1`・`TASK-29`・Issue #520）。この子プロセスは
    // `main`（`run_js_worker_if_requested` 経由）から本関数まで単一
    // スレッドで直線的に進み、Isolate に入るのもこのスレッドだけである
    // ため、`v8_engine::ensure_v8_initialized_with` のドキュメントコメント
    // が述べる「子孫スレッド制約」に抵触しない。呼ぶ位置は
    // `enforce_child_memory_limit()` の**後**（OS 側の上限を先に掛ける
    // 順序を保つ）・`V8Engine::new*` の**前**（Isolate 生成より前に
    // Platform を確定させる）にする。
    //
    // プロトコルバージョン検証の早期 return より**必ず下**に置くこと。
    // `js_1_worker_main_rejects_unsupported_protocol_version` 等の
    // ユニットテストはテストプロセス内で `worker_main` を直接呼ぶため、
    // これより上で protected 初期化を呼ぶとテストプロセスが protected に
    // なってしまう（他のテストスレッドが PKU ホストで `SEGV_PKUERR` に
    // なりうる。実装者への注意。Issue #520 実装計画）。
    let actual_platform =
        super::v8_engine::ensure_v8_initialized_with(super::v8_engine::V8PlatformKind::Protected);
    if actual_platform != super::v8_engine::V8PlatformKind::Protected {
        // 正規の子プロセスでは起こり得ない（このプロセスは V8 の Platform
        // をまだ初期化していないはずのため）。想定外の経路（テスト用の
        // 直接呼び出し等）で先に unprotected 初期化が行われていたことを
        // 示す。保護が有効であるかのように装わず、診断を出して評価は
        // 継続する（unprotected のままでも従来と同じ安全水準であり、
        // fail-closed で止める必要はない。security.md「偽装・回避機能の
        // 禁止」）。
        eprintln!(
            "fandhe-browser-js worker: V8 platform was already initialized as {actual_platform:?}; \
             thread-isolated allocation is not active"
        );
    }

    // 環境変数は untrusted な入力（`TEST_HEAP_LIMIT_ENV_VAR` のドキュメント
    // コメント参照）。`test_heap_limit_from_env_value` が読み取った直後に
    // クランプすることで、親を経由しない直接起動でも本番の上限
    // （`MAX_ISOLATE_HEAP_BYTES`）を超えられない。
    let heap_limit_override =
        test_heap_limit_from_env_value(std::env::var(TEST_HEAP_LIMIT_ENV_VAR).ok().as_deref());
    let engine_result = match heap_limit_override {
        Some(bytes) => V8Engine::new_with_heap_limit(bytes),
        None => V8Engine::new(),
    };
    let mut engine = match engine_result {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("fandhe-browser-js worker: failed to create the V8 engine: {err}");
            return ExitCode::FAILURE;
        }
    };

    let stdout = io::stdout();
    let mut stdout = stdout.lock();

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
        EngineKind::V8
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

    let stdin = io::stdin();
    let mut stdin = stdin.lock();

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
                if let Err(err) = evaluate_and_respond(&mut engine, &script, &mut stdout) {
                    eprintln!("fandhe-browser-js worker: failed to send a response frame: {err}");
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

/// 1 件の `Evaluate` を処理し、`Result`/`Error` フレームを書き返す。
///
/// 戻り値の `Err` はフレームの送信自体（I/O）が失敗したことを表す。
/// 評価そのものの失敗は `Error` フレームとして正常に送信し、`Ok(())` を
/// 返す（設計書 §3.2「タイムアウトでは子は死なず、Context もそのまま
/// 残る」。呼び出しループはこの後も継続する）。
fn evaluate_and_respond(
    engine: &mut V8Engine,
    script: &str,
    stdout: &mut impl Write,
) -> Result<(), ProtocolError> {
    match engine.evaluate_script(script, &EvaluateOptions::default()) {
        Ok(value) => {
            let mut payload = Vec::new();
            worker_protocol::encode_js_value(&value, &mut payload)?;
            send_frame(stdout, tag::RESULT, &payload)
        }
        Err(err) => {
            let (kind, message) = classify_evaluation_error(&err);
            let payload = worker_protocol::encode_error(kind, &message);
            send_frame(stdout, tag::ERROR, &payload)
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
/// `JsEngineError::ResourceLimitExceeded`・`JsEngineError::EngineUnavailable`
/// は現在の `v8_engine`（子プロセスの中で動く評価本体）実装からは返らない
/// （ヒープ上限到達はこのプロセス自体の fatal OOM に一本化し、
/// `EngineUnavailable` は親側 `process_engine.rs` だけが構築する variant
/// のため）。`enum` が存在する以上 `match` を尽くす必要があるため、
/// 防御的に「評価失敗」として扱う分岐を用意する（実際に到達する経路は
/// 無い。実装済みを装わない。REPAIR-3）。
fn classify_evaluation_error(err: &JsEngineError) -> (ErrorKind, String) {
    match err {
        JsEngineError::EvaluationFailed(msg) => (ErrorKind::Evaluation, truncate_for_wire(msg)),
        JsEngineError::BindingFailed(msg) => (ErrorKind::Binding, truncate_for_wire(msg)),
        JsEngineError::Timeout(msg) => (ErrorKind::Timeout, truncate_for_wire(msg)),
        JsEngineError::ResourceLimitExceeded(msg) | JsEngineError::EngineUnavailable(msg) => {
            (ErrorKind::Evaluation, truncate_for_wire(msg))
        }
    }
}

/// エラーメッセージを [`worker_protocol::MAX_ERROR_MESSAGE_BYTES`] まで
/// 切り詰める（バイト単位。文字境界を壊さないよう `str::is_char_boundary`
/// で境界を探してから切る。coding-rust.md「外部入力」節）。
fn truncate_for_wire(message: &str) -> String {
    if message.len() <= worker_protocol::MAX_ERROR_MESSAGE_BYTES {
        return message.to_string();
    }
    let mut end = worker_protocol::MAX_ERROR_MESSAGE_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.get(..end).unwrap_or_default().to_string()
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

    /// JS-1・Issue #503: 上限以内のメッセージはそのまま返すこと。
    #[test]
    fn js_1_truncate_for_wire_keeps_short_messages_untouched() {
        assert_eq!(truncate_for_wire("boom"), "boom");
    }

    /// JS-1・Issue #503: 上限を超えるメッセージは
    /// `MAX_ERROR_MESSAGE_BYTES` 以下（かつ文字境界で切られたバイト列）に
    /// 切り詰められること。
    #[test]
    fn js_1_truncate_for_wire_truncates_oversized_messages_at_a_char_boundary() {
        // マルチバイト文字（3 バイトの日本語）を境界ちょうどに配置し、
        // 境界探索が正しく機能することを確認する。
        let message = "a".repeat(worker_protocol::MAX_ERROR_MESSAGE_BYTES - 1) + "あ" + "b";
        let truncated = truncate_for_wire(&message);
        assert!(truncated.len() <= worker_protocol::MAX_ERROR_MESSAGE_BYTES);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    /// codex レビュー指摘 #503 P0: 親を経由しない直接起動で
    /// [`TEST_HEAP_LIMIT_ENV_VAR`] に本番の既定値（128 MiB =
    /// 134,217,728 バイト）を大きく超える値を渡しても、子プロセス自身が
    /// 既定値へクランプすること（実プロセス・stdio を介さず、値の変換
    /// 経路だけを具体値で検証する）。
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
    #[test]
    fn js_1_test_heap_limit_from_env_value_raises_zero_to_the_floor() {
        assert_eq!(test_heap_limit_from_env_value(Some("0")), Some(1_048_576));
    }

    /// codex レビュー指摘 #503 P0: `usize` として解釈できない値・値が
    /// 未設定の場合は `None`（既定のヒープ上限を使う経路）になること。
    #[test]
    fn js_1_test_heap_limit_from_env_value_falls_back_to_default_on_invalid_or_missing_input() {
        assert_eq!(test_heap_limit_from_env_value(Some("not-a-number")), None);
        assert_eq!(test_heap_limit_from_env_value(Some("")), None);
        assert_eq!(test_heap_limit_from_env_value(None), None);
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
        assert_eq!(kind, ErrorKind::Evaluation);
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
}
