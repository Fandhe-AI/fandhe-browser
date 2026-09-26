//! `fandhe-browser-core` の公開 HTML パース API（`parse_document`/
//! `parse_document_bytes`）を crate 外から検証する結合テスト
//! （TASK-24.4・#38・ビヘイビア `CORE-1`）。
//!
//! `dom::Document` のフィールドは `pub(crate)` のままだが、走査 API
//! （TASK-24.5・#39・`dom` モジュール）を経由すれば構造を検証できる
//! （`tests/dom.rs` が最小確認・`tests/parse_dom.rs`（TASK-24.6・#40）が
//! 代表的な HTML 断片による網羅的な構造検証を担当）。ここでは crate 外から
//! 見える契約（`Ok`/`Err` の判定・`Error` の `Display`・`Send + Sync` 境界）
//! のみを確認する。

use fandhe_browser_core::{Error, ParseError, ParseErrorPolicy, ParseOptions};

/// CORE-1: 整形式の HTML は `Ok` を返す。
#[test]
fn core_1_well_formed_html_returns_ok_from_outside_crate() {
    let input = "<!DOCTYPE html><html><head></head><body><p>hi</p></body></html>";
    let result = fandhe_browser_core::parse_document(input, &ParseOptions::default());
    assert!(result.is_ok());
}

/// CORE-1: 不正な UTF-8 バイト列は `Err(Error::Parse(ParseError::InvalidUtf8
/// { .. }))` を返し、panic しないことを確認する。
#[test]
fn core_1_invalid_utf8_bytes_return_err_without_panic() {
    let bytes: &[u8] = &[0xff, 0xfe];
    let err = fandhe_browser_core::parse_document_bytes(bytes, &ParseOptions::default())
        .expect_err("不正な UTF-8 は Err になる");
    assert!(matches!(
        err,
        Error::Parse(ParseError::InvalidUtf8 { valid_up_to: 0 })
    ));
}

/// CORE-1: `Error` の `Display` が crate 外から読める英語メッセージになる。
#[test]
fn core_1_error_display_from_outside_crate() {
    let bytes: &[u8] = &[0xff];
    let err = fandhe_browser_core::parse_document_bytes(bytes, &ParseOptions::default())
        .expect_err("不正な UTF-8 は Err になる");
    assert_eq!(
        err.to_string(),
        "parse error: invalid UTF-8 input (valid up to byte 0)"
    );
}

/// CORE-1: `ParseOptions` のビルダーメソッドが `non_exhaustive` な型でも
/// crate 外から構築できることを確認する。
#[test]
fn core_1_parse_options_builder_usable_from_outside_crate() {
    let options = ParseOptions::default()
        .with_max_input_bytes(1024)
        .with_max_nodes(100)
        .with_max_recorded_errors(5)
        .with_scripting_enabled(true)
        .with_error_policy(ParseErrorPolicy::Strict);

    let input = "<p>a<div>b</div>";
    let err = fandhe_browser_core::parse_document(input, &options)
        .expect_err("Strict ポリシーではパースエラーで Err になる");
    assert!(matches!(
        err,
        Error::Parse(ParseError::Malformed { error_count, .. }) if error_count > 0
    ));
}

/// CORE-1: `ParsedDocument`（`Ok` 時の戻り値）が `Send + Sync + 'static` を
/// 満たし、マルチスレッド環境へ持ち出せることを確認する。
#[test]
fn core_1_parsed_document_is_send_sync_from_outside_crate() {
    fn assert_bounds<T: Send + Sync + 'static>() {}
    assert_bounds::<fandhe_browser_core::ParsedDocument>();
}

/// CORE-1: `?` 演算子で `fandhe_browser_core::Result` へ panic なく伝播できる。
#[test]
fn core_1_parse_error_propagates_via_question_mark_without_panic() {
    fn caller() -> fandhe_browser_core::Result<()> {
        let bytes: &[u8] = &[0xff];
        fandhe_browser_core::parse_document_bytes(bytes, &ParseOptions::default())?;
        Ok(())
    }

    let err = caller().expect_err("不正な UTF-8 は Err になる");
    assert!(matches!(err, Error::Parse(ParseError::InvalidUtf8 { .. })));
}
