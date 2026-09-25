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
//! ## 構成（PR #427 レビュー指摘への対応。3 回目）
//!
//! 1 回目の対応（P0）で「ループが空でも成功扱いになる」問題への手当てを
//! 入れたが、その手当ては (a) 実処理コンフォーマンス検査
//! （[`conformance_checks`] モジュール内の `check_*` 一式）から `#[test]`
//! を外して「まだ配線されていない」状態にする、(b) 既定構成
//! （`js-v8`/`js-boa` 両 feature 無効）でのみ実行される契約テストは
//! ループが空のまま無検証で成功する、という 2 つの新しい P1 を生んでいた。
//! 2 回目の対応は cfg gate で「ループが空になるビルド構成そのものに
//! テストを存在させない」ことで両方を解消したが、その際に追加した
//! 既定構成テストの [`IMPLEMENTED_ENGINES`] 空アサーションが、
//! 「実装済みかどうか」と「現在のビルドに同梱されているかどうか」を
//! 混同する新たな P1（本コミットの対応対象）を生んでいた。本コミットは
//! 既定構成テストから [`IMPLEMENTED_ENGINES`] への参照を外し、
//! `bundled_engines()`/`NotBundled` のみで既定構成の契約を検証する形に
//! 直す。
//!
//! - [`js_1_create_engine_contract_for_bundled_engines`]（`js-v8`/`js-boa`
//!   のいずれかが有効な構成でのみ存在）は [`bundled_engines`] が非空である
//!   ことを前提にでき、`create_engine` が [`IMPLEMENTED_ENGINES`] の内容
//!   どおりに `Ok`/`NotYetImplemented` を返す契約を検証する。
//! - 既定構成（両 feature 無効）では代わりに
//!   [`js_1_create_engine_contract_is_empty_by_default`] が存在し、
//!   `bundled_engines()` が空であること・`create_engine` が V8・Boa
//!   いずれに対しても具体的に `NotBundled { bundled: [] }` を返すことを、
//!   値を伴って検証する（coding-rust.md「期待値は具体値で書く」）。
//!   [`IMPLEMENTED_ENGINES`] は「実装済みかどうか」と「現在のビルドに
//!   同梱されているかどうか」が独立した情報であるため、この既定構成
//!   テストでは参照しない（PR #427 レビュー指摘の 2 回目の対応）。
//! - [`conformance_checks`] モジュール（`check_*` を含む実処理検査一式）
//!   も同じ cfg gate 配下に置く。`js-v8`/`js-boa` いずれかが有効な限り
//!   3 関数すべてが `#[test]` として常に実行され、[`create_engine`] が
//!   `Ok` を返すようになった時点（TASK-29・V8／TASK-32・boa 完了時）で
//!   自動的に `check_*` を呼び出すようになる。「実装が入ったのに
//!   `#[test]` を付け直し忘れる」余地を構造上なくす（本 crate の CI は
//!   `cargo test --all-features` に加え既定 feature 構成も別ジョブで
//!   検証する。ci.yml 参照）。
//!
//! 現時点（[`IMPLEMENTED_ENGINES`] が空）では、feature 有効構成でも
//! `check_*` はまだ 1 度も実際のスクリプト評価まで到達しない
//! （`create_engine` がどの種別にも `NotYetImplemented` を返すため）。
//! これは実装状況をそのまま反映した結果であり、テスト自体は
//! `run_for_each_bundled_engine` を必ず呼び出し、[`bundled_engines`] を
//! 空にしない cfg gate と合わせて「ループが空のまま無検証で成功する」
//! ことを許さない（実装済みを装わない。REPAIR-3）。

use fandhe_browser_js::{CreateEngineError, EngineKind, bundled_engines, create_engine};

/// 具象実装が存在し、[`create_engine`] が `Ok` を返す種別の宣言
/// （TASK-29 完了時に [`EngineKind::V8`]、TASK-32 完了時に
/// [`EngineKind::Boa`] をここへ追加する）。
///
/// [`js_1_create_engine_contract_for_bundled_engines`]・
/// [`conformance_checks`] 内の各ヘルパーが期待値の唯一の情報源として参照
/// する（既定構成の
/// [`js_1_create_engine_contract_is_empty_by_default`] はこの定数を
/// 参照しない。下記参照）。ここに列挙されていない種別が `Ok` を返した
/// 場合、または列挙されて
/// いる種別がなお `NotYetImplemented` を返した場合はどちらもテスト失敗に
/// なる（実装状況とテストの期待値がずれたまま CI を通さない）。
///
/// 「実装済みかどうか」は「現在のビルドに同梱されているかどうか」
/// （`js-v8`/`js-boa` feature）と独立した情報のため、既定 feature 構成
/// （両無効）ではこの定数を参照しない（PR #427 レビュー指摘: 既定構成の
/// テストがこの定数を空であることを要求してしまうと、TASK-29/32 で
/// 実装済みエンジン種別をここへ追加した瞬間、既定構成（エンジンを同梱
/// しない契約自体は正しい）のテストが不当に失敗する。既定構成の検証は
/// [`js_1_create_engine_contract_is_empty_by_default`] が `bundled_engines()`
/// と `create_engine` の `NotBundled` 応答のみで行う）。そのため `js-v8`/
/// `js-boa` のいずれかが有効な構成でのみ存在させる。
#[cfg(any(feature = "js-v8", feature = "js-boa"))]
const IMPLEMENTED_ENGINES: &[EngineKind] = &[];

/// JS-1: `create_engine` の同梱種別ごとの契約（[`IMPLEMENTED_ENGINES`] に
/// 含まれるかどうかで `Ok`/`NotYetImplemented` のどちらを返すべきか）を
/// 検証する（TASK-28.4・Issue #150）。`js-v8`/`js-boa` のいずれかが有効な
/// 構成でのみ存在し、[`bundled_engines`] が非空であることを前提にできる
/// （既定構成の対応する検証は
/// [`js_1_create_engine_contract_is_empty_by_default`] を参照。PR #427
/// レビュー指摘: 空のループが無検証で成功しないよう、ループが空になり得る
/// 構成そのものに本テストを存在させない）。
#[test]
#[cfg(any(feature = "js-v8", feature = "js-boa"))]
fn js_1_create_engine_contract_for_bundled_engines() {
    let engines = bundled_engines();
    assert!(
        !engines.is_empty(),
        "this test is cfg-gated on js-v8/js-boa, so bundled_engines() must be non-empty here"
    );
    for &kind in engines {
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

/// JS-1: 既定 feature 構成（`js-v8`/`js-boa` 両無効）では
/// `bundled_engines()` が空であること・`create_engine` が V8・Boa
/// いずれに対しても具体的に `NotBundled { bundled: [] }` を返すことを
/// 検証する（TASK-28.4・Issue #150。PR #427 レビュー指摘: 既定構成で
/// 唯一実行される契約テストが空のループのまま無検証で成功していた問題
/// への対応。coding-rust.md「期待値は具体値で書く」に従い `is_err()`
/// ではなく具体的な列挙子・フィールド値を比較する）。
///
/// `IMPLEMENTED_ENGINES`（実装済みエンジン種別の宣言）は検証しない
/// （PR #427 レビュー指摘の 2 回目の対応: 「実装済みかどうか」と
/// 「現在のビルドに同梱されているかどうか」は独立した情報であり、
/// TASK-29/32 で実装済みエンジン種別をそこへ追加しても、既定構成が
/// エンジンを同梱しないという契約自体は変わらない。両者を混ぜて検証
/// すると、実装が進むたびに既定構成のこのテストが不当に失敗する）。
#[test]
#[cfg(not(any(feature = "js-v8", feature = "js-boa")))]
fn js_1_create_engine_contract_is_empty_by_default() {
    assert_eq!(
        bundled_engines(),
        &[] as &[EngineKind],
        "default build (js-v8/js-boa both disabled) must bundle no engines"
    );
    assert!(
        matches!(
            create_engine(EngineKind::V8),
            Err(CreateEngineError::NotBundled {
                requested: EngineKind::V8,
                bundled: []
            })
        ),
        "V8 must be reported as NotBundled with an empty bundled list by default"
    );
    assert!(
        matches!(
            create_engine(EngineKind::Boa),
            Err(CreateEngineError::NotBundled {
                requested: EngineKind::Boa,
                bundled: []
            })
        ),
        "Boa must be reported as NotBundled with an empty bundled list by default"
    );
}

/// `check_*`（実処理）を呼ぶコンフォーマンス検査一式（TASK-28.4）。
///
/// `js-v8`/`js-boa` のいずれかが有効な構成でのみコンパイル・実行される
/// （モジュール冒頭のドキュメント参照）。そのため [`bundled_engines`] が
/// 常に非空になる構成でのみ 3 関数すべてが `#[test]` として動く。
/// [`IMPLEMENTED_ENGINES`] が空の現時点では `create_engine` がどの種別にも
/// `NotYetImplemented` を返すため、`check_*`（実際のスクリプト評価・
/// 関数注入・DOM バインディング）自体はまだ 1 度も到達しない。ただし
/// `run_for_each_bundled_engine`（本モジュール内）は必ず呼ばれ、
/// 「同梱されているのに `NotYetImplemented` を返す」という現行契約を
/// 明示的に検証し続けるため、`cargo test` の出力が「pass」でも実際には
/// 何も検証していない、という状態にはならない（PR #427 レビュー指摘への
/// 対応。TASK-29・V8／TASK-32・boa の完了により対応する種別が
/// [`IMPLEMENTED_ENGINES`] へ追加された瞬間、既にある `#[test]` がそのまま
/// `check_*` を呼び出すようになる。付け直し忘れの余地がない）。
#[cfg(any(feature = "js-v8", feature = "js-boa"))]
mod conformance_checks {
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
    /// 突き合わせる共通ヘルパー（3 つの実行系関数から共通利用する）。
    ///
    /// 期待値はビルド構成に応じて動的に決まる: 同梱されている
    /// （[`bundled_engines`] に含まれる）種別のうち、[`IMPLEMENTED_ENGINES`] に
    /// 含まれるものは `checked` としてカウントされ、残りは
    /// `not_yet_implemented` としてカウントされるはずである
    /// （`IMPLEMENTED_ENGINES` を更新するだけで期待値が自動的に追従する。
    /// TASK-29/32 完了時にこのヘルパー自体は変更不要）。加えて、
    /// [`IMPLEMENTED_ENGINES`] が非空であるにもかかわらず `checked == 0` の
    /// まま（＝実装済みのはずの種別に対して `check_*` が一度も呼ばれない）
    /// 成功することを許さない（「検査対象があるのに無検証で成功しない」の
    /// 明示的な回帰確認）。`IMPLEMENTED_ENGINES` が空の現時点
    /// （`checked == 0` が正しい期待値）まで `checked > 0` を要求すると
    /// 本モジュールが常に失敗してしまうため、その場合はこの追加要求を
    /// 課さない（モジュール自体は cfg gate により `bundled_engines()` が
    /// 空にならない構成でのみ実行される。モジュールドキュメント参照）。
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
        if !IMPLEMENTED_ENGINES.is_empty() {
            assert!(
                summary.checked > 0,
                "IMPLEMENTED_ENGINES is non-empty but no conformance check_* was actually \
                 invoked (checked == 0); check the create_engine wiring for TASK-29/32"
            );
        }
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
    /// （TASK-28.4・Issue #150）。`js-v8`/`js-boa` のいずれかが有効な構成
    /// でのみ存在し、[`IMPLEMENTED_ENGINES`] が非空になった時点で
    /// `check_script_evaluation` を実際に呼び出すようになる
    /// （モジュールドキュメント参照）。
    #[test]
    fn js_1_conformance_script_evaluation_for_each_bundled_engine() {
        let summary = run_for_each_bundled_engine(check_script_evaluation);
        assert_conformance_summary_matches_current_contract(&summary);
    }

    /// JS-1: グローバル関数注入が、同梱された各エンジンで同じ形状で動作すること
    /// （TASK-28.4・Issue #150）。`js-v8`/`js-boa` のいずれかが有効な構成
    /// でのみ存在する（モジュールドキュメント参照）。
    #[test]
    fn js_1_conformance_global_function_injection_for_each_bundled_engine() {
        let summary = run_for_each_bundled_engine(check_global_function_injection);
        assert_conformance_summary_matches_current_contract(&summary);
    }

    /// JS-1: DOM 風オブジェクトへのバインディングが、同梱された各エンジンで
    /// 同じ形状で動作すること（TASK-28.4・Issue #150）。`js-v8`/`js-boa` の
    /// いずれかが有効な構成でのみ存在する（モジュールドキュメント参照）。
    #[test]
    fn js_1_conformance_dom_like_binding_for_each_bundled_engine() {
        let summary = run_for_each_bundled_engine(check_dom_like_binding);
        assert_conformance_summary_matches_current_contract(&summary);
    }
}
