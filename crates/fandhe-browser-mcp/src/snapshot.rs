//! snapshot ツールの応答解釈（TASK-94.4・PLUG-3・PLUG-4・MS-9）。
//!
//! `server.rs` の snapshot ハンドラから呼ばれる純関数群。ホストへの送信は `host.rs` が担う。
//! ホスト（ai crate）の `GET /ai/snapshot` は `{"url","tree","truncated"}` を返す。
//!
//! 将来仕様（REPAIR-3・AISNAP-6）: このエンベロープは暫定形で、スナップショットのスキーマ
//! 確定（AISNAP-6）に合わせて形状検証を更新する。トークン削減のためのテキスト形式の最適化は
//! TASK-96 / TASK-97（PLUG-4・PLUG-5）の測定結果を受けた別作業で、ここでは JSON の再直列化のみ。

use rmcp::serde_json::{self, Value};

/// `GET /ai/snapshot` のパス。
pub(crate) const SNAPSHOT_PATH: &str = "/ai/snapshot";
/// snapshot 応答の読み取り上限。stdio 1 メッセージ上限（`limit::MAX_MESSAGE_BYTES`）と揃える。
pub(crate) const MAX_SNAPSHOT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// 受理する JSON ネスト深度の上限。ホストの `MAX_TREE_DEPTH`（256）の木は 1 階層あたり
/// オブジェクトと `children` 配列の 2 ネストで、エンベロープ・葉の配列を含め約 520 になるため余裕を持たせる。
const MAX_JSON_NESTING: usize = 640;
/// ホストの `code` として保持する最大長。
const MAX_CODE_LEN: usize = 64;

/// ホスト応答の解釈結果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SnapshotOutcome {
    /// 検証済みスナップショットのコンパクト JSON テキスト。
    Success {
        text: String,
    },
    Failure {
        status: u16,
        code: Option<String>,
    },
}

/// 本文の JSON ネスト深度が上限以内かを反復的に走査して検証する（再帰しないためスタックを消費しない）。
/// 文字列内の括弧・エスケープは無視する。構文の正当性は後段のパーサーが検証するため、ここは深度のみ見る。
fn nesting_within_limit(body: &[u8]) -> bool {
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    for &b in body {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_JSON_NESTING {
                    return false;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    true
}

/// 深度検証済みの本文を、serde_json 既定の再帰上限（128）なしで解析する。
/// 事前検証で深度が [`MAX_JSON_NESTING`] 以内と分かっているためスタック枯渇しない。
fn parse_bounded(body: &[u8]) -> Option<Value> {
    if !nesting_within_limit(body) {
        return None;
    }
    let mut de = serde_json::Deserializer::from_slice(body);
    de.disable_recursion_limit();
    let mut it = de.into_iter::<Value>();
    let v = it.next()?.ok()?;
    // 後続に余分なデータがあれば（次要素の解析結果が存在すれば）不正として扱う。
    if it.next().is_some() {
        return None;
    }
    Some(v)
}

/// ホスト応答を解釈する。2xx かつエンベロープ形状が正しいときのみ成功（fail-closed）。
/// 失敗時はホスト本文の自由文を流さず、安全な `code` のみ保持する。
pub(crate) fn interpret(status: u16, body: &[u8]) -> SnapshotOutcome {
    let parsed: Option<Value> = parse_bounded(body);
    if (200..300).contains(&status)
        && let Some(v) = parsed.as_ref()
        && v.get("url").is_some_and(Value::is_string)
        && v.get("tree").is_some_and(Value::is_object)
        && v.get("truncated").is_some_and(Value::is_boolean)
        && let Ok(text) = serde_json::to_string(v)
    {
        return SnapshotOutcome::Success { text };
    }
    let code = parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(Value::as_str)
        .filter(|c| {
            (1..=MAX_CODE_LEN).contains(&c.len())
                && c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        })
        .map(str::to_owned);
    SnapshotOutcome::Failure { status, code }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(status: u16, code: Option<&str>) -> SnapshotOutcome {
        SnapshotOutcome::Failure {
            status,
            code: code.map(str::to_owned),
        }
    }

    /// PLUG-4 / TASK-94.4: 正しいエンベロープは成功し、再解析すると同じ内容。
    #[test]
    fn plug4_interpret_success() {
        let body =
            br#"{"url":"https://e.com/","tree":{"role":"document"},"truncated":false,"extra":1}"#;
        let SnapshotOutcome::Success { text } = interpret(200, body) else {
            panic!("expected success");
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["url"], "https://e.com/");
        assert_eq!(v["tree"]["role"], "document");
        assert_eq!(v["truncated"], false);
        assert_eq!(v["extra"], 1);
    }

    /// PLUG-4 / TASK-94.4: ホストの失敗応答は code のみ保持する。
    #[test]
    fn plug4_interpret_host_failure() {
        assert_eq!(
            interpret(409, br#"{"code":"no_navigation","message":"SECRET"}"#),
            fail(409, Some("no_navigation"))
        );
        assert_eq!(interpret(500, br#"{"code":"Bad Code!"}"#), fail(500, None));
        let env = br#"{"url":"u","tree":{},"truncated":true}"#;
        assert_eq!(interpret(500, env), fail(500, None));
    }

    /// PLUG-4 / TASK-94.4: 2xx でも形の違う・切れた本文は失敗にする。
    #[test]
    fn plug4_interpret_malformed_is_failure() {
        for body in [
            &b"not json"[..],
            br#"{"url":"u","tree":{"#,
            br#"[]"#,
            br#"{"url":"u","truncated":false}"#,
            br#"{"url":"u","tree":{},"truncated":"no"}"#,
            br#"{"url":1,"tree":{},"truncated":false}"#,
        ] {
            assert_eq!(interpret(200, body), fail(200, None));
        }
    }

    fn nested_tree(levels: usize) -> Vec<u8> {
        // ホストの node_json と同形（各階層 = オブジェクト + children 配列）。
        let mut tree = String::from(r#"{"role":"leaf","children":[]}"#);
        for _ in 0..levels {
            tree = format!(r#"{{"role":"n","children":[{tree}]}}"#);
        }
        format!(r#"{{"url":"u","tree":{tree},"truncated":false}}"#).into_bytes()
    }

    /// PLUG-4 / TASK-94.4: ホスト最大深度（256）の木を受理し、128 超でも成功する。
    #[test]
    fn plug4_interpret_accepts_host_max_depth() {
        for levels in [64, 128, 256] {
            let body = nested_tree(levels);
            let SnapshotOutcome::Success { text } = interpret(200, &body) else {
                panic!("levels={levels} should succeed");
            };
            assert_eq!(text.len(), body.len());
        }
    }

    /// PLUG-4 / TASK-94.4: 深度上限超過は明示的な Failure（スタック枯渇しない）。
    #[test]
    fn plug4_interpret_rejects_excess_depth() {
        let body = nested_tree(400);
        assert_eq!(interpret(200, &body), fail(200, None));
        let deep = format!("{}{}", "[".repeat(1_000_000), "]".repeat(1_000_000));
        assert_eq!(interpret(200, deep.as_bytes()), fail(200, None));
    }

    /// 文字列中の括弧・エスケープは深度に数えない。
    #[test]
    fn plug4_nesting_ignores_brackets_in_strings() {
        let s = "[\\\"".repeat(2000);
        let body = format!(r#"{{"url":"{s}","tree":{{}},"truncated":false}}"#);
        assert!(matches!(
            interpret(200, body.as_bytes()),
            SnapshotOutcome::Success { .. }
        ));
    }
}
