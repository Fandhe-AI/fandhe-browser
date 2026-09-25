//! `fandhe-browser-core` の公開エラー型（`Error`/`Result`）を crate 外から
//! 検証する結合テスト（TASK-24（24.1）・ビヘイビア `CORE-1`）。
//!
//! `fetch`/`parse`/`dom`/`query` の各実装（#36/#38/#39/#41）は、この
//! `fandhe_browser_core::Result` を戻り値として `?` 演算子でエラーを
//! 伝播させる想定であり、その経路が panic なく機能することを確認する。

use fandhe_browser_core::{Error, Result};

/// 呼び出し元へ `?` でエラーを伝播させる、外部入力検証を模した関数。
fn validate(input: &str) -> Result<()> {
    if input.is_empty() {
        return Err(Error::InvalidInput {
            message: "input must not be empty".to_string(),
        });
    }
    Ok(())
}

/// CORE-1: `Result` 経由でエラーが panic なく `?` 伝播することを確認する。
#[test]
fn core_1_result_propagates_via_question_mark_without_panic() {
    fn caller() -> Result<()> {
        validate("")?;
        Ok(())
    }

    let err = caller().expect_err("空文字列は InvalidInput になるはず");
    assert_eq!(err.to_string(), "invalid input: input must not be empty");
}

/// CORE-1: 正常系では `Result::Ok` がそのまま返ることを確認する。
#[test]
fn core_1_result_ok_on_valid_input() {
    assert_eq!(
        validate("https://example.com").map_err(|e| e.to_string()),
        Ok(())
    );
}

/// CORE-1: crate 外から見た `Error` の `Display` 文字列を確認する。
#[test]
fn core_1_error_display_from_outside_crate() {
    let err = Error::Unsupported {
        message: "gzip encoding".to_string(),
    };
    assert_eq!(err.to_string(), "unsupported: gzip encoding");
}

/// CORE-1: crate 外から見た `Error` の `std::error::Error` 実装
/// （`source()` を含む）を確認する。
#[test]
fn core_1_error_trait_object_from_outside_crate() {
    let io_err = std::io::Error::other("boom");
    let err: Box<dyn std::error::Error> = Box::new(Error::Io(io_err));
    assert_eq!(err.to_string(), "I/O error: boom");
    assert_eq!(
        err.source().map(ToString::to_string),
        Some("boom".to_string())
    );
}
