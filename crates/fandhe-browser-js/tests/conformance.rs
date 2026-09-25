//! `JsEngine` 実装が満たすべき共通コンフォーマンステスト（TASK-28
//! （28.4）・Issue #150・ビヘイビア `JS-1`・MS-3）。
//!
//! `docs/spec/04-behavior/js-engine.md`「JS エンジンの切替方式」決定 5 は、
//! V8・boa が同じ API 形状（[`JsEngine`] トレイト）で差し替え可能である
//! ことを求める。本ファイルはその「差し替え可能性」を検証する側の入口で
//! あり、[`bundled_engines`] が返す各エンジン種別に対して同じ検査を流す
//! （検査内容は `docs/spec/03-poc/js-engine-comparison` の PoC-3 で実測した
//! `print`・`dom.setText`/`dom.getText`/`dom.count` の形状を下敷きにする）。
//! TASK-64（コンテナのスモークテスト）は同等の検査をコンテナ内で実行する
//! 想定であり、本ファイルの検査内容がその受け皿になる。
//!
//! ## 構成（PR #427 レビュー指摘への対応）
//!
//! 常に実行される [`js_1_create_engine_contract_for_bundled_engines`] は、
//! [`create_engine`] が [`IMPLEMENTED_ENGINES`] の内容どおりに
//! `Ok`/`NotYetImplemented` を返す契約自体を検証する。
//!
//! 一方、実際に `check_*`（スクリプト評価・グローバル関数注入・DOM 風
//! バインディング）を呼ぶ実処理コンフォーマンス検査は
//! [`pending_conformance_checks`] モジュール内に隔離している。理由:
//! `IMPLEMENTED_ENGINES` が空の間（TASK-29・V8／TASK-32・boa がまだ未完了）は
//! `create_engine` がどの種別に対しても `NotYetImplemented` を返すため、
//! `js-v8`/`js-boa` feature（本 crate の CI は `cargo test --all-features`
//! でも検証する。ci.yml 参照）を有効にしても `check_*` は一度も呼ばれない。
//! この状態で `#[test]` として実行し続けると、`cargo test` の出力上は
//! 「pass」と表示されながら実際には何も検査していない状態が固定化し、
//! 将来の回帰を検出できなくなる（AGENTS.md「回帰検出の後退」規約に抵触。
//! `#[ignore]` による skip は coding-rust.md で禁止されているため、
//! ignore で隠す選択肢は取らない）。そのため `check_*` を呼ぶ実行系一式は
//! `#[test]` を外し、`cargo test` の集計に一切現れない「まだ配線されていない
//! テスト支援 API」として保持する（実装済みを装わない。REPAIR-3）。
//!
//! 申し送り: TASK-29（V8）・TASK-32（boa）の完了後、[`IMPLEMENTED_ENGINES`]
//! へ当該 [`EngineKind`] を追加したうえで、
//! [`pending_conformance_checks`] 内の 3 関数（
//! `js_1_conformance_script_evaluation_for_each_bundled_engine` 等）に
//! `#[test]` を付け直し、モジュール先頭の `#![allow(dead_code)]` を外す。
//! それまでの間、このモジュールを実際に呼び出す `#[test]` は存在しない。

use fandhe_browser_js::{CreateEngineError, EngineKind, bundled_engines, create_engine};

/// 具象実装が存在し、[`create_engine`] が `Ok` を返す種別の宣言
/// （TASK-29 完了時に [`EngineKind::V8`]、TASK-32 完了時に
/// [`EngineKind::Boa`] をここへ追加する）。
///
/// [`js_1_create_engine_contract_for_bundled_engines`]・
/// [`pending_conformance_checks`] 内の各ヘルパーが期待値の唯一の情報源
/// として参照する。ここに列挙されていない種別が `Ok` を返した場合、
/// または列挙されている種別がなお `NotYetImplemented` を返した場合は
/// どちらもテスト失敗になる（実装状況とテストの期待値がずれたまま CI を
/// 通さない）。
const IMPLEMENTED_ENGINES: &[EngineKind] = &[];

/// JS-1: `create_engine` の同梱種別ごとの契約（[`IMPLEMENTED_ENGINES`] に
/// 含まれるかどうかで `Ok`/`NotYetImplemented` のどちらを返すべきか）を
/// 検証する（TASK-28.4・Issue #150）。`IMPLEMENTED_ENGINES` が空の間も
/// 「同梱種別は全て `NotYetImplemented` を返す」という現行契約自体を
/// 検査し続けるため、`bundled_engines()` が空でない構成（`js-v8`/`js-boa`
/// feature 有効時）では常に意味のある検査になる。
#[test]
fn js_1_create_engine_contract_for_bundled_engines() {
    for &kind in bundled_engines() {
        let result = create_engine(kind);
        if IMPLEMENTED_ENGINES.contains(&kind) {
            assert!(
                result.is_ok(),
                "{kind:?} is listed in IMPLEMENTED_ENGINES, so create_engine must return Ok, \
                 got an Err instead"
            );
        } else {
            assert!(
                matches!(
                    result,
                    Err(CreateEngineError::NotYetImplemented { requested }) if requested == kind
                ),
                "{kind:?} is not listed in IMPLEMENTED_ENGINES, so create_engine must return \
                 NotYetImplemented{{ requested: {kind:?} }}, got something else"
            );
        }
    }
}

/// `check_*`（実処理）を呼ぶコンフォーマンス検査一式（TASK-28.4）。
///
/// モジュール冒頭のドキュメント参照: [`IMPLEMENTED_ENGINES`] が空の現時点
/// では、このモジュール内のどの関数にも `#[test]` を付けていない。付ける
/// と `cargo test` が「検査対象 0 件のまま成功」を報告し続けてしまうため
/// （PR #427 レビュー指摘）、TASK-29/32 で実装が入り
/// `IMPLEMENTED_ENGINES` を更新するタイミングまでは、この一式を
/// テスト実行の対象外に置く（`#[ignore]` ではなくモジュール分離＋
/// `#[test]` 不在によって対象外にする。理由はモジュールドキュメント参照）。
// dead_code: TASK-29/32 完了後、IMPLEMENTED_ENGINES 更新と同時に #[test] を
// 付け直して使う（モジュールドキュメント参照。`reason` 付き allow はナイトリー
// 限定の lint_reasons feature が必要なため、stable toolchain 前提の本 crate
// ではコメントで理由を示す）。
#[allow(dead_code)]
mod pending_conformance_checks {
    use super::IMPLEMENTED_ENGINES;
    use fandhe_browser_js::{
        CreateEngineError, EngineKind, EvaluateOptions, JsEngine, JsEngineError, JsValue, NativeFn,
        bundled_engines, create_engine,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// [`run_for_each_bundled_engine`] の実行結果（何件を実チェックし、何件が
    /// `NotYetImplemented` 契約確認で終わったか）。
    ///
    /// 呼び出し元は [`assert_conformance_summary_matches_current_contract`] で
    /// この値を検証し、「ループが 0 回だったので何も検査していない」ことを
    /// 暗黙のまま素通りさせない。
    #[derive(Debug, PartialEq, Eq)]
    struct ConformanceRunSummary {
        /// `create_engine` が `Ok` を返し、`check` を実際に呼び出した件数。
        checked: usize,
        /// `CreateEngineError::NotYetImplemented` 契約確認で終わった件数。
        not_yet_implemented: usize,
    }

    /// [`bundled_engines`] が返す各エンジン種別に対して `check` を実行する
    /// ドライバ（TASK-28.4）。
    ///
    /// - `Ok` の場合はチェック関数へトレイトオブジェクトを渡す（TASK-29/32
    ///   完了後、この分岐が実際にスクリプトを評価するようになる）
    /// - `NotYetImplemented` の場合は、要求した種別と一致すること、かつその
    ///   種別が [`IMPLEMENTED_ENGINES`] に含まれていないことを確認する
    ///   （現行契約（[`create_engine`] のドキュメント参照）の明示的な回帰
    ///   確認。`IMPLEMENTED_ENGINES` に追加した種別がなお
    ///   `NotYetImplemented` を返す場合はここで失敗する）
    /// - それ以外の `Err`（`NotBundled` を含む。[`bundled_engines`] 由来の種別で
    ///   起きてはならない）は `panic!` で失敗させる（fail-closed。
    ///   成功を装うフォールバックを行わない。security.md「偽装・回避機能の禁止」）
    ///
    /// 戻り値の [`ConformanceRunSummary`] は、呼び出し元が「実際に何件検査
    /// したか」を明示的にアサートするための情報を運ぶ。
    fn run_for_each_bundled_engine(
        check: fn(EngineKind, &mut dyn JsEngine),
    ) -> ConformanceRunSummary {
        let mut checked = 0usize;
        let mut not_yet_implemented = 0usize;
        for &kind in bundled_engines() {
            match create_engine(kind) {
                Ok(mut engine) => {
                    check(kind, engine.as_mut());
                    checked += 1;
                }
                Err(CreateEngineError::NotYetImplemented { requested }) => {
                    assert_eq!(
                        requested, kind,
                        "NotYetImplemented must report the kind it was requested for"
                    );
                    assert!(
                        !IMPLEMENTED_ENGINES.contains(&kind),
                        "{kind:?} is listed in IMPLEMENTED_ENGINES but create_engine still \
                         returned NotYetImplemented; update the create_engine match arm"
                    );
                    not_yet_implemented += 1;
                }
                Err(other) => {
                    panic!("create_engine({kind:?}) failed unexpectedly: {other}");
                }
            }
        }
        ConformanceRunSummary {
            checked,
            not_yet_implemented,
        }
    }

    /// [`ConformanceRunSummary`] を [`IMPLEMENTED_ENGINES`] から導出した期待値と
    /// 突き合わせ、かつ「検査対象 0 件のまま成功」を許さない共通ヘルパー
    /// （3 つの実行系関数から共通利用する。PR #427 レビュー指摘への対応）。
    ///
    /// 期待値はビルド構成に応じて動的に決まる: 同梱されている
    /// （[`bundled_engines`] に含まれる）種別のうち、[`IMPLEMENTED_ENGINES`] に
    /// 含まれるものは `checked` としてカウントされ、残りは
    /// `not_yet_implemented` としてカウントされるはずである
    /// （`IMPLEMENTED_ENGINES` を更新するだけで期待値が自動的に追従する。
    /// TASK-29/32 完了時にこのヘルパー自体は変更不要）。加えて、このモジュール
    /// が `#[test]` として実行される状態（TASK-29/32 完了後）になった時点で
    /// `checked == 0` のまま成功することがないよう、`checked > 0` も明示的に
    /// 要求する（「検査対象が 0 件のときに成功扱いにならない」というレビュー
    /// 指摘そのものへの対応）。
    fn assert_conformance_summary_matches_current_contract(summary: &ConformanceRunSummary) {
        let expected_checked = bundled_engines()
            .iter()
            .filter(|kind| IMPLEMENTED_ENGINES.contains(kind))
            .count();
        let expected_not_yet_implemented = bundled_engines().len() - expected_checked;
        assert_eq!(
            summary.checked, expected_checked,
            "checked must equal the number of bundled engines listed in IMPLEMENTED_ENGINES"
        );
        assert_eq!(
            summary.not_yet_implemented, expected_not_yet_implemented,
            "not_yet_implemented must equal the number of bundled engines NOT listed in \
             IMPLEMENTED_ENGINES"
        );
        assert!(
            summary.checked > 0,
            "conformance check_* functions were never invoked (checked == 0); this test must \
             not be re-enabled with #[test] until at least one engine is implemented \
             (IMPLEMENTED_ENGINES is non-empty)"
        );
    }

    /// `JS-1`「スクリプト評価」のコンフォーマンス検査（エンジン非依存）。
    fn check_script_evaluation(kind: EngineKind, engine: &mut dyn JsEngine) {
        let options = EvaluateOptions::default();

        let result = engine
            .evaluate_script("1 + 2 * 3", &options)
            .unwrap_or_else(|err| panic!("[{kind:?}] arithmetic evaluation failed: {err}"));
        assert_eq!(result, JsValue::Number(7.0), "[{kind:?}] arithmetic result");

        let result = engine
            .evaluate_script("'fandhe' + '-' + 'browser'", &options)
            .unwrap_or_else(|err| panic!("[{kind:?}] string concatenation failed: {err}"));
        assert_eq!(
            result,
            JsValue::String("fandhe-browser".into()),
            "[{kind:?}] string concatenation result"
        );

        let result = engine
            .evaluate_script("true", &options)
            .unwrap_or_else(|err| panic!("[{kind:?}] bool literal evaluation failed: {err}"));
        assert_eq!(
            result,
            JsValue::Bool(true),
            "[{kind:?}] bool literal result"
        );

        let result = engine
            .evaluate_script("null", &options)
            .unwrap_or_else(|err| panic!("[{kind:?}] null literal evaluation failed: {err}"));
        assert_eq!(result, JsValue::Null, "[{kind:?}] null literal result");

        let result = engine
            .evaluate_script("undefined", &options)
            .unwrap_or_else(|err| panic!("[{kind:?}] undefined literal evaluation failed: {err}"));
        assert_eq!(
            result,
            JsValue::Undefined,
            "[{kind:?}] undefined literal result"
        );

        let result = engine.evaluate_script("1 +", &options);
        assert!(
            matches!(result, Err(JsEngineError::EvaluationFailed(_))),
            "[{kind:?}] syntax error must yield EvaluationFailed, got {result:?}"
        );

        let result = engine.evaluate_script("throw new Error('boom')", &options);
        assert!(
            matches!(result, Err(JsEngineError::EvaluationFailed(_))),
            "[{kind:?}] runtime exception must yield EvaluationFailed, got {result:?}"
        );
    }

    /// `JS-1`「グローバル関数注入」のコンフォーマンス検査（エンジン非依存）。
    fn check_global_function_injection(kind: EngineKind, engine: &mut dyn JsEngine) {
        let options = EvaluateOptions::default();

        // print: Rust 側が呼び出し引数を記録できること（PoC-3 の print 相当）。
        let printed: Rc<RefCell<Vec<JsValue>>> = Rc::new(RefCell::new(Vec::new()));
        let printed_for_closure = Rc::clone(&printed);
        let print_fn: NativeFn = Box::new(move |args: &[JsValue]| {
            printed_for_closure.borrow_mut().extend_from_slice(args);
            Ok(JsValue::Undefined)
        });
        engine
            .inject_global_function("print", print_fn)
            .unwrap_or_else(|err| panic!("[{kind:?}] injecting print failed: {err}"));
        engine
            .evaluate_script("print('hello')", &options)
            .unwrap_or_else(|err| panic!("[{kind:?}] calling print failed: {err}"));
        assert_eq!(
            *printed.borrow(),
            vec![JsValue::String("hello".into())],
            "[{kind:?}] print must record its argument on the Rust side"
        );

        // add: 戻り値が JS へ往復すること。添字アクセスは使わず get() で取り出す
        // （coding-rust.md「外部入力の経路では添字アクセスを使わない」の手本）。
        let add_fn: NativeFn = Box::new(|args: &[JsValue]| match (args.first(), args.get(1)) {
            (Some(JsValue::Number(a)), Some(JsValue::Number(b))) => Ok(JsValue::Number(a + b)),
            _ => Err(JsEngineError::BindingFailed(
                "add expects two numbers".into(),
            )),
        });
        engine
            .inject_global_function("add", add_fn)
            .unwrap_or_else(|err| panic!("[{kind:?}] injecting add failed: {err}"));
        let result = engine
            .evaluate_script("add(2, 3)", &options)
            .unwrap_or_else(|err| panic!("[{kind:?}] calling add failed: {err}"));
        assert_eq!(result, JsValue::Number(5.0), "[{kind:?}] add(2, 3) result");

        // ネイティブ関数側のエラーが評価結果へ伝播すること。具体バリアントは
        // エンジン実装依存の余地があるため、ここでは「成功値を返さない」
        // （is_err）ことのみを確認する。
        let failing_fn: NativeFn =
            Box::new(|_args: &[JsValue]| Err(JsEngineError::BindingFailed("always fails".into())));
        engine
            .inject_global_function("alwaysFails", failing_fn)
            .unwrap_or_else(|err| panic!("[{kind:?}] injecting alwaysFails failed: {err}"));
        let result = engine.evaluate_script("alwaysFails()", &options);
        assert!(
            result.is_err(),
            "[{kind:?}] calling a native fn that errors must not yield a success value, got \
             {result:?}"
        );
    }

    /// `JS-1`「DOM 風オブジェクトへのバインディング」のコンフォーマンス検査
    /// （エンジン非依存。PoC-3 の `dom.setText`/`dom.getText`/`dom.count` 相当）。
    fn check_dom_like_binding(kind: EngineKind, engine: &mut dyn JsEngine) {
        let options = EvaluateOptions::default();
        let store: Rc<RefCell<HashMap<String, String>>> = Rc::new(RefCell::new(HashMap::new()));

        let set_text_fn: NativeFn = {
            let store = Rc::clone(&store);
            Box::new(move |args: &[JsValue]| match (args.first(), args.get(1)) {
                (Some(JsValue::String(id)), Some(JsValue::String(text))) => {
                    store.borrow_mut().insert(id.clone(), text.clone());
                    Ok(JsValue::Undefined)
                }
                _ => Err(JsEngineError::BindingFailed(
                    "setText expects (id, text) strings".into(),
                )),
            })
        };
        let get_text_fn: NativeFn = {
            let store = Rc::clone(&store);
            Box::new(move |args: &[JsValue]| match args.first() {
                Some(JsValue::String(id)) => Ok(JsValue::String(
                    store.borrow().get(id).cloned().unwrap_or_default(),
                )),
                _ => Err(JsEngineError::BindingFailed(
                    "getText expects an id string".into(),
                )),
            })
        };
        let count_fn: NativeFn = {
            let store = Rc::clone(&store);
            Box::new(move |_args: &[JsValue]| Ok(JsValue::Number(store.borrow().len() as f64)))
        };

        engine
            .bind_dom_like_object(
                "dom",
                vec![
                    ("setText".to_string(), set_text_fn),
                    ("getText".to_string(), get_text_fn),
                    ("count".to_string(), count_fn),
                ],
            )
            .unwrap_or_else(|err| panic!("[{kind:?}] binding dom object failed: {err}"));

        let result = engine
            .evaluate_script(
                "dom.setText('title', 'Fandhe'); dom.getText('title')",
                &options,
            )
            .unwrap_or_else(|err| {
                panic!("[{kind:?}] dom.setText/getText round-trip failed: {err}")
            });
        assert_eq!(
            result,
            JsValue::String("Fandhe".into()),
            "[{kind:?}] dom.getText('title') result"
        );

        let result = engine
            .evaluate_script("dom.setText('body', 'x'); dom.count()", &options)
            .unwrap_or_else(|err| panic!("[{kind:?}] dom.count failed: {err}"));
        assert_eq!(
            result,
            JsValue::Number(2.0),
            "[{kind:?}] dom.count() result"
        );

        assert_eq!(
            store.borrow().get("title").map(String::as_str),
            Some("Fandhe"),
            "[{kind:?}] Rust-side store must reflect dom.setText('title', ...)"
        );
        assert_eq!(
            store.borrow().get("body").map(String::as_str),
            Some("x"),
            "[{kind:?}] Rust-side store must reflect dom.setText('body', ...)"
        );

        let result = engine
            .evaluate_script("dom.getText('missing')", &options)
            .unwrap_or_else(|err| panic!("[{kind:?}] dom.getText(missing id) failed: {err}"));
        assert_eq!(
            result,
            JsValue::String(String::new()),
            "[{kind:?}] dom.getText for an unregistered id must be an empty string"
        );
    }

    /// JS-1: スクリプト評価が、同梱された各エンジンで同じ形状の結果を返すこと
    /// （TASK-28.4・Issue #150）。TASK-29/32 完了後に `#[test]` を付け直す
    /// （モジュールドキュメント参照）。
    fn js_1_conformance_script_evaluation_for_each_bundled_engine() {
        let summary = run_for_each_bundled_engine(check_script_evaluation);
        assert_conformance_summary_matches_current_contract(&summary);
    }

    /// JS-1: グローバル関数注入が、同梱された各エンジンで同じ形状で動作すること
    /// （TASK-28.4・Issue #150）。TASK-29/32 完了後に `#[test]` を付け直す
    /// （モジュールドキュメント参照）。
    fn js_1_conformance_global_function_injection_for_each_bundled_engine() {
        let summary = run_for_each_bundled_engine(check_global_function_injection);
        assert_conformance_summary_matches_current_contract(&summary);
    }

    /// JS-1: DOM 風オブジェクトへのバインディングが、同梱された各エンジンで
    /// 同じ形状で動作すること（TASK-28.4・Issue #150）。TASK-29/32 完了後に
    /// `#[test]` を付け直す（モジュールドキュメント参照）。
    fn js_1_conformance_dom_like_binding_for_each_bundled_engine() {
        let summary = run_for_each_bundled_engine(check_dom_like_binding);
        assert_conformance_summary_matches_current_contract(&summary);
    }
}
