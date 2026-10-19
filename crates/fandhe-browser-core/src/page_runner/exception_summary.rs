//! JS 例外の要約（分類名と不足識別子名）を、生メッセージを残さずに作る（`JS-5`・`REPAIR-4`・Issue #1133）。
//!
//! 呼び出し元: [`super::module_graph`] の module 評価失敗（静的 root・動的 `import()`）。
//! 診断に出してよいのは固定語彙の enum と、文字種・段数・長さを検証した識別子だけで、
//! メッセージ本文・URL・クエリ・specifier は残さない。V8（`Uncaught Kind: ...`）と
//! boa（接頭辞なし）の両方の形式を受ける。classic の例外は現状対象外（`REPAIR-3`）。

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
}

impl ExceptionSummary {
    /// 種類も型も特定できない要約（件数の対応を保つための穴埋めに使う）。
    pub(crate) fn other() -> Self {
        Self {
            kind: ExceptionKind::Other,
            class: ExceptionMessageClass::Other,
            identifier: None,
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
    let (class, ident) = if let Some(x) = body.strip_suffix(" is not defined") {
        (ExceptionMessageClass::NotDefined, identifier_of(x))
    } else if let Some(x) = body.strip_suffix(" is not a function") {
        (ExceptionMessageClass::NotAFunction, identifier_of(x))
    } else if let Some(x) = body.strip_suffix(" is not a constructor") {
        (ExceptionMessageClass::NotAConstructor, identifier_of(x))
    } else if body.starts_with("Cannot read properties of") {
        (ExceptionMessageClass::ReadProperty, None)
    } else if let Some((_, tail)) = body.split_once("does not provide an export named '") {
        let name = tail.split('\'').next().unwrap_or("");
        (ExceptionMessageClass::MissingExport, identifier_of(name))
    } else {
        (ExceptionMessageClass::Other, None)
    };
    let kind = if kind == ExceptionKind::Other && class == ExceptionMessageClass::NotDefined {
        ExceptionKind::ReferenceError
    } else {
        kind
    };
    ExceptionSummary {
        kind,
        class,
        identifier: ident,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(m: &str) -> (ExceptionKind, ExceptionMessageClass, Option<String>) {
        let r = summarize_exception_message(m);
        (r.kind, r.class, r.identifier)
    }

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

    #[test]
    fn unknown_kind_is_other() {
        assert_eq!(
            s("Uncaught FooError: x"),
            (ExceptionKind::Other, ExceptionMessageClass::Other, None)
        );
    }

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
}
