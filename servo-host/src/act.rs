//! 操作モードの `act`・`wait` で評価する固定のスクリプトと、結果の解釈（TASK-126・`RENDER-10`・`AISNAP-18`。
//! 設計 `docs/design/servo-host-design.md` §14.4・§14.5）。
//!
//! 役割: `page` の `ServoHandler` が、要素の照合・フォーカスの確認・`select` の選択肢の引き当て・待機の条件を
//! `evaluate_javascript` で調べ、`select` の選択肢を設定するためのスクリプトを作る。値（要素 ID・照合値・`values`・セレクタ・テキスト）は
//! `serde_json` で JSON のリテラルにしてから固定の関数式へ渡し、連結しない（確定条件 8）。スクリプトはページの
//! グローバルへ書き込まない。ページは prototype や `window.webdriverElement` を書き換えられるため、結果は自己申告
//! として扱う（照合・フォーカスの確認は、操作してよいかの判定にだけ使う。設計 §14.4）。
//!
//! 入力文字・キーはスクリプトに入れない（入力イベントで渡す。`page` 側）。

use serde_json::{Value, json};
use servo::{JSValue, Key};

use crate::server::{ActExpect, WaitCondition};

/// 照合の結果（[`expect_script`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpectOutcome {
    /// 照合値が一致し、`disabled` でない。
    Ok,
    /// 要素が消えた、または照合値が違う（`stale`）。
    Stale,
    /// `disabled`（`not_actionable`）。
    Disabled,
}

/// 番号の要素を `window.webdriverElement(id)` で引き直し、照合値と `disabled` を調べるスクリプト。
pub(crate) fn expect_script(element_id: &str, expect: &ActExpect) -> String {
    let e = json!({
        "tag": expect.tag,
        "id": expect.id,
        "name": expect.name,
        "type": expect.r#type,
    });
    format!(
        "(function(id,e){{var el=(typeof window.webdriverElement==='function')?window.webdriverElement(id):null;\
if(!el||!el.isConnected)return 'stale';\
if(el.localName!==e.tag)return 'stale';\
var ks=['id','name','type'];for(var i=0;i<ks.length;i++){{var k=ks[i];if(e[k]!==null&&el.getAttribute(k)!==e[k])return 'stale';}}\
if(el.disabled===true||(el.closest&&el.closest('fieldset[disabled]')&&el.localName!=='legend'))return 'disabled';\
return 'ok';}})({},{})",
        Value::from(element_id),
        e
    )
}

/// [`expect_script`] の結果を解釈する。想定外の値は `Stale`（推測で操作しない）。
pub(crate) fn parse_expect(v: &JSValue) -> ExpectOutcome {
    match v {
        JSValue::String(s) if s == "ok" => ExpectOutcome::Ok,
        JSValue::String(s) if s == "disabled" => ExpectOutcome::Disabled,
        _ => ExpectOutcome::Stale,
    }
}

/// 番号の要素にフォーカスがあるか（`document.activeElement` と同じか）を返すスクリプト。
pub(crate) fn focus_script(element_id: &str) -> String {
    format!(
        "(function(id){{var el=(typeof window.webdriverElement==='function')?window.webdriverElement(id):null;\
return !!el&&document.activeElement===el;}})({})",
        Value::from(element_id)
    )
}

/// `select` の選択肢を `values` にするスクリプト。結果は `ok`・`stale`（要素が無い・`select` でない）・`missing`
/// （一致する選択肢が無い）・`multi`（単一選択に 2 つ以上）。
///
/// Servo 0.7.0 の WebDriver の `ElementClick` は、単一選択の `select` の `option` を押しても選択を変えない
/// （`servo-script` の `handle_element_click` の Step 8.6.3 は `multiple` のときだけ切り替える）。そこで選択肢は
/// スクリプトで設定し、`input`・`change` を出す。このイベントはスクリプトが作るため `isTrusted` は `false`
/// （既知の制約。設計 §14.4）。複数選択では `values` に含まれる選択肢だけを選んだ状態にする。
pub(crate) fn select_script(select_id: &str, values: &[String]) -> String {
    format!(
        "(function(id,vs){{var el=(typeof window.webdriverElement==='function')?window.webdriverElement(id):null;\
if(!el||el.localName!=='select')return 'stale';\
if(!el.multiple&&vs.length!==1)return 'multi';\
var o=el.options,hit={{}};for(var i=0;i<o.length;i++){{if(vs.indexOf(o[i].value)>=0)hit[o[i].value]=true;}}\
for(var j=0;j<vs.length;j++){{if(!hit[vs[j]])return 'missing';}}\
if(el.multiple){{for(var k=0;k<o.length;k++){{o[k].selected=vs.indexOf(o[k].value)>=0;}}}}\
else{{for(var m=0;m<o.length;m++){{if(o[m].value===vs[0]){{o[m].selected=true;break;}}}}}}\
el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));\
return 'ok';}})({},{})",
        Value::from(select_id),
        Value::from(values.to_vec())
    )
}

/// 待機の条件を 1 回調べるスクリプト（真偽値を返す。`load_complete` は `page` が読み込みの状態で判定する）。
pub(crate) fn condition_script(c: &WaitCondition) -> Option<String> {
    Some(match c {
        WaitCondition::SelectorPresent(s) => format!(
            "(function(s){{try{{return document.querySelector(s)!==null;}}catch(e){{return 'invalid';}}}})({})",
            Value::from(s.as_str())
        ),
        WaitCondition::SelectorAbsent(s) => format!(
            "(function(s){{try{{return document.querySelector(s)===null;}}catch(e){{return 'invalid';}}}})({})",
            Value::from(s.as_str())
        ),
        WaitCondition::TextPresent(t) => format!(
            "(function(t){{var b=document.body||document.documentElement;return !!b&&String(b.innerText||b.textContent||'').indexOf(t)>=0;}})({})",
            Value::from(t.as_str())
        ),
        WaitCondition::LoadComplete => return None,
    })
}

/// 条件スクリプトの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConditionOutcome {
    Met,
    NotYet,
    /// セレクタの構文が不正（`invalid_params`）。
    Invalid,
}

pub(crate) fn parse_condition(v: &JSValue) -> ConditionOutcome {
    match v {
        JSValue::Boolean(true) => ConditionOutcome::Met,
        JSValue::String(s) if s == "invalid" => ConditionOutcome::Invalid,
        _ => ConditionOutcome::NotYet,
    }
}

/// `press` の `key` を解釈する。名前つきのキー（`Enter`・`Tab` 等。`Unidentified` は除く）か、1 文字。
/// それ以外は `None`（`invalid_params`）。
pub(crate) fn parse_key(key: &str) -> Option<Key> {
    let k: Key = key.parse().ok()?;
    match &k {
        Key::Character(s) if s.chars().count() == 1 => Some(k),
        Key::Named(servo::NamedKey::Unidentified) => None,
        Key::Named(_) => Some(k),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect(tag: &str) -> ActExpect {
        ActExpect {
            tag: tag.to_owned(),
            id: None,
            name: Some("q\"');alert(1)//".to_owned()),
            r#type: None,
        }
    }

    /// 確定条件 8: 値は JSON のリテラルとして渡し、スクリプトに生で連結しない。
    #[test]
    fn scripts_embed_values_as_json_literals() {
        let s = expect_script("id\"'</script>", &expect("input"));
        assert!(s.contains(r#""id\"'</script>""#), "{s}");
        assert!(s.contains(r#""name":"q\"');alert(1)//""#), "{s}");
        let o = select_script("x", &["v\"".to_owned()]);
        assert!(o.ends_with(r#"("x",["v\""])"#), "{o}");
        let c = condition_script(&WaitCondition::SelectorPresent("a\"b".into())).unwrap();
        assert!(c.ends_with(r#"("a\"b")"#), "{c}");
        assert!(condition_script(&WaitCondition::LoadComplete).is_none());
    }

    #[test]
    fn results_are_parsed_fail_closed() {
        assert_eq!(
            parse_expect(&JSValue::String("ok".into())),
            ExpectOutcome::Ok
        );
        assert_eq!(
            parse_expect(&JSValue::String("disabled".into())),
            ExpectOutcome::Disabled
        );
        assert_eq!(parse_expect(&JSValue::Boolean(true)), ExpectOutcome::Stale);
        assert_eq!(
            parse_condition(&JSValue::Boolean(true)),
            ConditionOutcome::Met
        );
        assert_eq!(
            parse_condition(&JSValue::Boolean(false)),
            ConditionOutcome::NotYet
        );
        assert_eq!(
            parse_condition(&JSValue::String("invalid".into())),
            ConditionOutcome::Invalid
        );
    }

    #[test]
    fn keys_are_named_or_single_characters() {
        assert_eq!(parse_key("Enter"), Some(Key::Named(servo::NamedKey::Enter)));
        assert_eq!(parse_key("a"), Some(Key::Character("a".into())));
        assert_eq!(parse_key("あ"), Some(Key::Character("あ".into())));
        assert_eq!(parse_key("ab"), None);
        assert_eq!(parse_key("Unidentified"), None);
        assert_eq!(parse_key("NotAKey"), None);
    }
}
