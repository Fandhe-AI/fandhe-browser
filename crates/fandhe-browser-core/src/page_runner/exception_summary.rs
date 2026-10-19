//! JS 例外の要約（分類名と不足識別子名）を、生メッセージを残さずに作る（`JS-5`・`REPAIR-4`・Issue #1133）。
//!
//! 呼び出し元: [`super::module_graph`] の module 評価失敗（静的 root・動的 `import()`）。
//! 診断に出してよいのは固定語彙の enum と、文字種・段数・長さを検証した識別子だけで、
//! メッセージ本文・URL・クエリ・specifier は残さない。V8（`Uncaught Kind: ...`）と
//! boa（接頭辞なし）の両方の形式を受ける。classic の例外は現状対象外（`REPAIR-3`）。
//!
//! `TypeError` の `other` を減らすため、`Cannot use 'in' operator ...`（V8）・
//! `right-hand side of 'in' should be an object ...`（boa）と `Cannot set properties of ...`（V8）を
//! 分類する（Issue #1140）。調べられた・設定されたキー名は [`ExceptionSummary::property`] に
//! 検証済みの場合だけ入れる。boa には「set properties」に当たる文言が無く、`in` の
//! キー名もメッセージに含まれないため、boa ではそれぞれ `other`／名前なしになる（`REPAIR-3`）。
//! `Cannot convert undefined or null to object` は将来の分類候補で、現状は `other` のまま。
//!
//! React の本番ビルドが投げる `Minified React error #N; visit <URL> ...` は
//! [`ExceptionMessageClass::MinifiedReactError`] に分類し、エラー番号 `N` だけを
//! [`ExceptionSummary::code`] に検証済み（ASCII 数字 1〜6 桁）の場合に入れる。案内 URL・
//! 引数は残さない（Issue #1145）。この定型は `Error` としてしか投げられないため、
//! 種類の接頭辞が無い boa でも `Error` と推定する。番号が検証外でも分類は保つ（`REPAIR-3`）。

/// エラー番号（`code`）の最大桁数。
const MAX_ERROR_CODE_DIGITS: usize = 6;
/// 識別子 1 段あたりの最大バイト数。
const MAX_IDENTIFIER_SEGMENT_BYTES: usize = 64;
/// 識別子の最大段数（`a.b.c` で 3 段）。
const MAX_EXCEPTION_IDENTIFIER_SEGMENTS: usize = 8;
/// 識別子全体の最大バイト数。
const MAX_EXCEPTION_IDENTIFIER_BYTES: usize = 128;
/// 分類器が読むメッセージの先頭バイト数（巨大入力でも走査量を固定する）。
const MAX_SCANNED_MESSAGE_BYTES: usize = 1024;

/// 例外の種類（JS の組み込みエラー名。語彙外は [`Self::Other`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExceptionKind {
    /// `ReferenceError`。
    ReferenceError,
    /// `TypeError`。
    TypeError,
    /// `SyntaxError`。
    SyntaxError,
    /// `RangeError`。
    RangeError,
    /// `EvalError`。
    EvalError,
    /// `URIError`。
    UriError,
    /// `AggregateError`。
    AggregateError,
    /// `Error`。
    Error,
    /// 語彙外、または種類を特定できない。
    Other,
}

impl ExceptionKind {
    /// 診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReferenceError => "ReferenceError",
            Self::TypeError => "TypeError",
            Self::SyntaxError => "SyntaxError",
            Self::RangeError => "RangeError",
            Self::EvalError => "EvalError",
            Self::UriError => "URIError",
            Self::AggregateError => "AggregateError",
            Self::Error => "Error",
            Self::Other => "other",
        }
    }

    fn parse(name: &str) -> Self {
        match name {
            "ReferenceError" => Self::ReferenceError,
            "TypeError" => Self::TypeError,
            "SyntaxError" => Self::SyntaxError,
            "RangeError" => Self::RangeError,
            "EvalError" => Self::EvalError,
            "URIError" => Self::UriError,
            "AggregateError" => Self::AggregateError,
            "Error" => Self::Error,
            _ => Self::Other,
        }
    }
}

/// メッセージの型（固定語彙。本文は残さない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExceptionMessageClass {
    /// `X is not defined`。
    NotDefined,
    /// `X is not a function`。
    NotAFunction,
    /// `X is not a constructor`。
    NotAConstructor,
    /// `Cannot read properties of ...`。
    ReadProperty,
    /// `The requested module ... does not provide an export named 'X'`。
    MissingExport,
    /// `Cannot use 'in' operator to search for 'K' in undefined`（V8）／
    /// `right-hand side of 'in' should be an object`（boa）。右辺の値は残さない。
    CannotUseInOperator,
    /// `Cannot set properties of undefined (setting 'K')`（V8）。
    CannotSetProperties,
    /// React 本番ビルドの `Minified React error #N; visit ...`。番号は
    /// [`ExceptionSummary::code`] に検証済みの場合だけ入る（Issue #1145）。
    MinifiedReactError,
    /// 上記以外。
    Other,
}

impl ExceptionMessageClass {
    /// 診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotDefined => "not_defined",
            Self::NotAFunction => "not_a_function",
            Self::NotAConstructor => "not_a_constructor",
            Self::ReadProperty => "read_property",
            Self::MissingExport => "missing_export",
            Self::CannotUseInOperator => "cannot_use_in_operator",
            Self::CannotSetProperties => "cannot_set_properties",
            Self::MinifiedReactError => "minified_react_error",
            Self::Other => "other",
        }
    }
}

/// JS 例外の要約（生メッセージ・URL・specifier を含まない。`JS-5`・`REPAIR-4`・Issue #1133）。
///
/// 現状は module 評価失敗でのみ作られる（classic は将来。`REPAIR-3`）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExceptionSummary {
    kind: ExceptionKind,
    class: ExceptionMessageClass,
    identifier: Option<String>,
    property: Option<String>,
    code: Option<String>,
}

impl ExceptionSummary {
    /// 種類も型も特定できない要約（件数の対応を保つための穴埋めに使う）。
    pub(crate) fn other() -> Self {
        Self {
            kind: ExceptionKind::Other,
            class: ExceptionMessageClass::Other,
            identifier: None,
            property: None,
            code: None,
        }
    }

    /// エンジンの例外メッセージから要約を作る（メッセージ自体は保持しない）。
    ///
    /// core 内では module 評価失敗の経路が使う。ハーネスの自己テストが実エンジンなしで
    /// 要約を組み立てるためにも公開する。
    pub fn from_engine_message(message: &str) -> Self {
        summarize_exception_message(message)
    }

    /// 例外の種類。
    pub fn kind(&self) -> ExceptionKind {
        self.kind
    }

    /// メッセージの型。
    pub fn message_class(&self) -> ExceptionMessageClass {
        self.class
    }

    /// 検証を通った不足識別子名（`window.foo` 等。特定できなければ `None`）。
    pub fn identifier(&self) -> Option<&str> {
        self.identifier.as_deref()
    }

    /// 例外メッセージ中の、読み取り・設定・`in` 検索の対象になったプロパティ名（検証済み）。
    /// 不足 API 名ではない（`identifier` とは別物）。特定・検証できなければ `None`。
    pub fn property(&self) -> Option<&str> {
        self.property.as_deref()
    }

    /// ライブラリのエラー番号（`Minified React error #N` の `N`。ASCII 数字 1〜6 桁だけ）。
    /// 不足 API 名（`identifier`）とも property とも別物。特定・検証できなければ `None`。
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }
}

/// `Minified React error #` の直後から番号を取り出して検証する（数字 1〜6 桁、直後は
/// 終端・`;`・空白のいずれか。満たさなければ `None`）。
fn react_error_code(tail: &str) -> Option<String> {
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    let digits = tail.get(..end)?;
    if digits.is_empty() || digits.len() > MAX_ERROR_CODE_DIGITS {
        return None;
    }
    match tail.get(end..).and_then(|r| r.chars().next()) {
        None | Some(';') | Some(' ') => Some(digits.to_string()),
        _ => None,
    }
}

/// `'` で囲まれた最初の区間を取り出して検証する（閉じ `'` が無ければ `None`）。
fn quoted_property(tail: &str) -> Option<String> {
    let (name, _) = tail.split_once('\'')?;
    identifier_of(name)
}

/// 識別子の検証（段ごとに `[A-Za-z_$][A-Za-z0-9_$]{0,63}`、最大 8 段・128 バイト）。
fn is_valid_identifier(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_EXCEPTION_IDENTIFIER_BYTES {
        return false;
    }
    let mut segments = 0usize;
    for seg in s.split('.') {
        segments += 1;
        if segments > MAX_EXCEPTION_IDENTIFIER_SEGMENTS || seg.len() > MAX_IDENTIFIER_SEGMENT_BYTES
        {
            return false;
        }
        let mut chars = seg.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
            _ => return false,
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
            return false;
        }
    }
    true
}

fn identifier_of(s: &str) -> Option<String> {
    is_valid_identifier(s).then(|| s.to_string())
}

/// 先頭 [`MAX_SCANNED_MESSAGE_BYTES`] バイト（char 境界）に切る。
fn clip(msg: &str) -> &str {
    if msg.len() <= MAX_SCANNED_MESSAGE_BYTES {
        return msg;
    }
    let mut end = MAX_SCANNED_MESSAGE_BYTES;
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    msg.get(..end).unwrap_or("")
}

/// エンジンの例外メッセージを要約する（純関数。panic しない）。
///
/// V8 形式 `Uncaught Kind: body (line N)` と boa 形式 `body` を受ける。
/// `X is not defined` は kind が不明なら `ReferenceError` と推定する（boa は接頭辞を付けない）。
pub(crate) fn summarize_exception_message(msg: &str) -> ExceptionSummary {
    let msg = clip(msg);
    let msg = msg.strip_prefix("Uncaught ").unwrap_or(msg);
    let (kind, body) = match msg.split_once(": ") {
        Some((name, rest)) if ExceptionKind::parse(name) != ExceptionKind::Other => {
            (ExceptionKind::parse(name), rest)
        }
        _ => (ExceptionKind::Other, msg),
    };
    let body = match body.rfind(" (line ") {
        Some(pos) if body.ends_with(')') => body.get(..pos).unwrap_or(body),
        _ => body,
    };
    let mut property = None;
    let mut code = None;
    let (class, ident) = if let Some(x) = body.strip_suffix(" is not defined") {
        (ExceptionMessageClass::NotDefined, identifier_of(x))
    } else if let Some(x) = body.strip_suffix(" is not a function") {
        (ExceptionMessageClass::NotAFunction, identifier_of(x))
    } else if let Some(x) = body.strip_suffix(" is not a constructor") {
        (ExceptionMessageClass::NotAConstructor, identifier_of(x))
    } else if body.starts_with("Cannot read properties of") {
        if let Some((_, tail)) = body.split_once("(reading '") {
            property = quoted_property(tail);
        }
        (ExceptionMessageClass::ReadProperty, None)
    } else if let Some(tail) = body.strip_prefix("Cannot use 'in' operator to search for '") {
        property = quoted_property(tail);
        (ExceptionMessageClass::CannotUseInOperator, None)
    } else if body.starts_with("right-hand side of 'in' should be an object") {
        (ExceptionMessageClass::CannotUseInOperator, None)
    } else if body.starts_with("Cannot set properties of") {
        if let Some((_, tail)) = body.split_once("(setting '") {
            property = quoted_property(tail);
        }
        (ExceptionMessageClass::CannotSetProperties, None)
    } else if let Some((_, tail)) = body.split_once("does not provide an export named '") {
        let name = tail.split('\'').next().unwrap_or("");
        (ExceptionMessageClass::MissingExport, identifier_of(name))
    } else if let Some(tail) = body.strip_prefix("Minified React error #") {
        code = react_error_code(tail);
        (ExceptionMessageClass::MinifiedReactError, None)
    } else {
        (ExceptionMessageClass::Other, None)
    };
    // boa は種類の接頭辞を付けないため、固有の文言から推定する。
    let kind = match (kind, class) {
        (ExceptionKind::Other, ExceptionMessageClass::NotDefined) => ExceptionKind::ReferenceError,
        (ExceptionKind::Other, ExceptionMessageClass::CannotUseInOperator) => {
            ExceptionKind::TypeError
        }
        (ExceptionKind::Other, ExceptionMessageClass::MinifiedReactError) => ExceptionKind::Error,
        _ => kind,
    };
    ExceptionSummary {
        kind,
        class,
        identifier: ident,
        property,
        code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(m: &str) -> (ExceptionKind, ExceptionMessageClass, Option<String>) {
        let r = summarize_exception_message(m);
        (r.kind, r.class, r.identifier)
    }

    /// V8 形式の ReferenceError から不足識別子名を取り出す（`JS-5`・`REPAIR-4`）。
    #[test]
    fn v8_not_defined_with_line() {
        assert_eq!(
            s("Uncaught ReferenceError: IntersectionObserver is not defined (line 3)"),
            (
                ExceptionKind::ReferenceError,
                ExceptionMessageClass::NotDefined,
                Some("IntersectionObserver".into())
            )
        );
    }

    /// boa 形式（接頭辞なし）から ReferenceError を推定する（`JS-5`・`REPAIR-4`）。
    #[test]
    fn boa_not_defined_infers_reference_error() {
        assert_eq!(
            s("customElements is not defined"),
            (
                ExceptionKind::ReferenceError,
                ExceptionMessageClass::NotDefined,
                Some("customElements".into())
            )
        );
    }

    /// `a.b is not a function` の段数付き識別子を取り出す（`JS-5`・`REPAIR-4`）。
    #[test]
    fn not_a_function_dotted() {
        assert_eq!(
            s("Uncaught TypeError: window.foo.bar is not a function"),
            (
                ExceptionKind::TypeError,
                ExceptionMessageClass::NotAFunction,
                Some("window.foo.bar".into())
            )
        );
    }

    /// プロパティ読み取り失敗は識別子を残さない（`JS-5`・`REPAIR-4`）。
    #[test]
    fn read_property_has_no_identifier() {
        assert_eq!(
            s("Uncaught TypeError: Cannot read properties of undefined (reading 'x') (line 2)"),
            (
                ExceptionKind::TypeError,
                ExceptionMessageClass::ReadProperty,
                None
            )
        );
    }

    /// missing export は specifier を残さない（`JS-5`・`REPAIR-4`）。
    #[test]
    fn missing_export_drops_specifier() {
        let r = summarize_exception_message(
            "Uncaught SyntaxError: The requested module './d.js?token=SECRET' does not provide an export named 'nope'",
        );
        assert_eq!(r.kind(), ExceptionKind::SyntaxError);
        assert_eq!(r.message_class(), ExceptionMessageClass::MissingExport);
        assert_eq!(r.identifier(), Some("nope"));
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("SECRET") && !dbg.contains("d.js") && !dbg.contains("token"));
    }

    /// URL を含むメッセージは other に分類し漏らさない（`JS-5`・`REPAIR-4`）。
    #[test]
    fn url_message_is_other_and_not_leaked() {
        let r =
            summarize_exception_message("Uncaught Error: https://h.example/p?token=SECRET_TOKEN");
        assert_eq!(r.kind(), ExceptionKind::Error);
        assert_eq!(r.message_class(), ExceptionMessageClass::Other);
        assert_eq!(r.identifier(), None);
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("SECRET_TOKEN") && !dbg.contains("https"));
    }

    /// 不正な識別子は None になる（`JS-5`・`REPAIR-4`）。
    #[test]
    fn invalid_identifiers_become_none() {
        let long_seg = "a".repeat(65);
        let nine = ["a"; 9].join(".");
        let b129 = format!("{}.{}.{}", "a".repeat(60), "b".repeat(60), "c".repeat(9));
        for bad in [
            long_seg.as_str(),
            nine.as_str(),
            b129.as_str(),
            "a-b",
            "(intermediate value).x",
            "",
            "1a",
        ] {
            let r = summarize_exception_message(&format!("{bad} is not defined"));
            assert_eq!(
                r.message_class(),
                ExceptionMessageClass::NotDefined,
                "{bad}"
            );
            assert_eq!(r.identifier(), None, "{bad}");
        }
    }

    /// 未知の例外種別は other になる（`JS-5`・`REPAIR-4`）。
    #[test]
    fn unknown_kind_is_other() {
        assert_eq!(
            s("Uncaught FooError: x"),
            (ExceptionKind::Other, ExceptionMessageClass::Other, None)
        );
    }

    /// 巨大入力でも長さが上限内に収まる（`JS-5`・`REPAIR-4`）。
    #[test]
    fn huge_input_is_bounded() {
        let big = format!(
            "Uncaught ReferenceError: {} is not defined",
            "a".repeat(1 << 20)
        );
        let r = summarize_exception_message(&big);
        assert!(r.identifier().is_none_or(|i| i.len() <= 128));
        let multibyte = "あ".repeat(5000);
        let _ = summarize_exception_message(&multibyte);
    }

    fn sp(
        m: &str,
    ) -> (
        ExceptionKind,
        ExceptionMessageClass,
        Option<String>,
        Option<String>,
    ) {
        let r = summarize_exception_message(m);
        (r.kind, r.class, r.identifier, r.property)
    }

    /// V8 の `in` 演算子の右辺が undefined / null（`JS-5`・`REPAIR-4`）。
    #[test]
    fn v8_in_operator_extracts_property() {
        for rhs in ["undefined", "null"] {
            assert_eq!(
                sp(&format!(
                    "Uncaught TypeError: Cannot use 'in' operator to search for 'WebkitAnimation' in {rhs} (line 1)"
                )),
                (
                    ExceptionKind::TypeError,
                    ExceptionMessageClass::CannotUseInOperator,
                    None,
                    Some("WebkitAnimation".into())
                )
            );
        }
    }

    /// boa の `in` は名前なしで TypeError と推定する（`JS-5`・`REPAIR-4`・`REPAIR-3`）。
    #[test]
    fn boa_in_operator_infers_type_error() {
        assert_eq!(
            sp("right-hand side of 'in' should be an object, got `undefined`"),
            (
                ExceptionKind::TypeError,
                ExceptionMessageClass::CannotUseInOperator,
                None,
                None
            )
        );
    }

    /// V8 のプロパティ設定失敗（`JS-5`・`REPAIR-4`）。
    #[test]
    fn v8_set_properties_extracts_property() {
        assert_eq!(
            sp("Uncaught TypeError: Cannot set properties of undefined (setting 'x') (line 4)"),
            (
                ExceptionKind::TypeError,
                ExceptionMessageClass::CannotSetProperties,
                None,
                Some("x".into())
            )
        );
    }

    /// read_property でも property を取り、identifier は None のまま（`JS-5`・`REPAIR-4`）。
    #[test]
    fn read_property_has_property_only() {
        assert_eq!(
            sp("Uncaught TypeError: Cannot read properties of undefined (reading 'x') (line 2)"),
            (
                ExceptionKind::TypeError,
                ExceptionMessageClass::ReadProperty,
                None,
                Some("x".into())
            )
        );
    }

    /// 検証外のキーは分類を保ったまま property が None（`JS-5`・`REPAIR-4`）。
    #[test]
    fn invalid_property_keys_become_none() {
        let long_seg = "a".repeat(65);
        let nine = ["a"; 9].join(".");
        for bad in [
            "-webkit-animation",
            long_seg.as_str(),
            nine.as_str(),
            "",
            "a b",
            "https://h.example/p?token=SECRET",
        ] {
            let r = summarize_exception_message(&format!(
                "Uncaught TypeError: Cannot use 'in' operator to search for '{bad}' in undefined"
            ));
            assert_eq!(
                r.message_class(),
                ExceptionMessageClass::CannotUseInOperator,
                "{bad}"
            );
            assert_eq!(r.property(), None, "{bad}");
            let dbg = format!("{r:?}");
            assert!(!dbg.contains("SECRET") && !dbg.contains("https"), "{bad}");
        }
    }

    /// 閉じ引用符なし・巨大・マルチバイトでも panic せず上限内（`JS-5`・`REPAIR-4`）。
    #[test]
    fn property_is_bounded_and_panic_free() {
        let r = summarize_exception_message("Cannot use 'in' operator to search for 'abc");
        assert_eq!(
            r.message_class(),
            ExceptionMessageClass::CannotUseInOperator
        );
        assert_eq!(r.property(), None);
        let big = format!(
            "Uncaught TypeError: Cannot set properties of undefined (setting '{}')",
            "a".repeat(1 << 20)
        );
        assert_eq!(summarize_exception_message(&big).property(), None);
        let ok = summarize_exception_message(&format!(
            "Cannot set properties of undefined (setting '{}')",
            "あ".repeat(5000)
        ));
        assert!(ok.property().is_none_or(|p| p.len() <= 128));
    }

    /// 語彙外の boa 文言は other のまま固定し、新旧語彙を確認する（`JS-5`・`REPAIR-4`・`REPAIR-3`）。
    #[test]
    fn convert_to_object_stays_other_and_vocabulary_extended() {
        assert_eq!(
            sp("cannot convert 'null' or 'undefined' to object").1,
            ExceptionMessageClass::Other
        );
        assert_eq!(
            ExceptionMessageClass::CannotUseInOperator.as_str(),
            "cannot_use_in_operator"
        );
        assert_eq!(
            ExceptionMessageClass::CannotSetProperties.as_str(),
            "cannot_set_properties"
        );
        assert_eq!(ExceptionMessageClass::NotDefined.as_str(), "not_defined");
        assert_eq!(
            ExceptionMessageClass::NotAFunction.as_str(),
            "not_a_function"
        );
    }

    /// as_str の語彙が固定である（`JS-5`・`REPAIR-4`）。
    #[test]
    fn as_str_vocabulary_is_fixed() {
        assert_eq!(ExceptionKind::UriError.as_str(), "URIError");
        assert_eq!(ExceptionKind::Other.as_str(), "other");
        assert_eq!(
            ExceptionMessageClass::MissingExport.as_str(),
            "missing_export"
        );
        assert_eq!(
            ExceptionMessageClass::NotAConstructor.as_str(),
            "not_a_constructor"
        );
        assert_eq!(
            ExceptionMessageClass::ReadProperty.as_str(),
            "read_property"
        );
    }

    /// V8 形式の Minified React error（番号だけを取り出す。`JS-5`・`REPAIR-4`・Issue #1145）。
    #[test]
    fn v8_minified_react_error_extracts_code() {
        let r = summarize_exception_message(
            "Uncaught Error: Minified React error #123; visit https://h.example/errors/123?token=SECRET for the full message. (line 9)",
        );
        assert_eq!(r.kind(), ExceptionKind::Error);
        assert_eq!(r.message_class(), ExceptionMessageClass::MinifiedReactError);
        assert_eq!(r.code(), Some("123"));
        assert_eq!(r.identifier(), None);
        assert_eq!(r.property(), None);
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("SECRET") && !dbg.contains("https") && !dbg.contains("token"));
    }

    /// boa 形式（接頭辞なし）でも分類・番号を取り、種類は Error と推定する
    /// （`JS-5`・`REPAIR-4`・`REPAIR-3`・Issue #1145）。
    #[test]
    fn boa_minified_react_error_infers_error() {
        let r = summarize_exception_message("Minified React error #45; visit https://h.example/x");
        assert_eq!(r.kind(), ExceptionKind::Error);
        assert_eq!(r.message_class(), ExceptionMessageClass::MinifiedReactError);
        assert_eq!(r.code(), Some("45"));
        // 番号だけで終わる形でも取れる。
        assert_eq!(
            summarize_exception_message("Minified React error #7").code(),
            Some("7")
        );
    }

    /// 検証外の番号は分類を保ったまま code が None（`JS-5`・`REPAIR-4`・Issue #1145）。
    #[test]
    fn invalid_react_error_codes_become_none() {
        for bad in [
            "",
            "1234567",
            "12a",
            "-1",
            "１２",
            "https://h.example/?token=SECRET",
            "0x1",
        ] {
            let r = summarize_exception_message(&format!(
                "Uncaught Error: Minified React error #{bad}; visit https://h.example"
            ));
            assert_eq!(
                r.message_class(),
                ExceptionMessageClass::MinifiedReactError,
                "{bad}"
            );
            assert_eq!(r.code(), None, "{bad}");
            let dbg = format!("{r:?}");
            assert!(!dbg.contains("SECRET") && !dbg.contains("https"), "{bad}");
        }
        // 6 桁は上限内。
        assert_eq!(
            summarize_exception_message("Minified React error #999999").code(),
            Some("999999")
        );
    }

    /// 巨大入力・マルチバイトでも panic せず上限内（`JS-5`・`REPAIR-4`・Issue #1145）。
    #[test]
    fn react_error_code_is_bounded_and_panic_free() {
        let big = format!(
            "Uncaught Error: Minified React error #{}",
            "9".repeat(1 << 20)
        );
        assert_eq!(summarize_exception_message(&big).code(), None);
        let multi = format!("Minified React error #{}", "あ".repeat(5000));
        assert_eq!(summarize_exception_message(&multi).code(), None);
        assert_eq!(
            summarize_exception_message("Minified React error #").code(),
            None
        );
    }

    /// 新しい語彙の表記と、既存の Error 文言が `other` のままであること（Issue #1145）。
    #[test]
    fn minified_react_error_vocabulary_is_fixed() {
        assert_eq!(
            ExceptionMessageClass::MinifiedReactError.as_str(),
            "minified_react_error"
        );
        let r = summarize_exception_message("Uncaught Error: something else");
        assert_eq!(r.message_class(), ExceptionMessageClass::Other);
        assert_eq!(r.code(), None);
        assert_eq!(ExceptionSummary::other().code(), None);
    }
}
