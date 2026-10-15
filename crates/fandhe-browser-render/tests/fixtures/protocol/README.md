# レンダリングホスト プロトコル ゴールデン fixture

本体（`fandhe-browser-render` の IPC クライアント）と Servo ホスト（別 workspace の
`servo-host/`）は型を共有しない。両側の契約の正本は次の 2 つで、各側の契約テストが
同じ fixture を読む（TASK-38・`PLUG-1`・`SEC-2`・`REPAIR-4`）。

- スキーマ: `docs/design/render-host-protocol.schema.json`
- fixture: 本ディレクトリ（`<direction>-<case>.json`）

型・コーデックの実装は #989（クライアント）・#993（ホスト）の範囲。ホスト側の契約テストが
本ディレクトリを読む配線も #993・#995 の範囲。

## 封筒（envelope）

```json
{
  "case": "load-ok",
  "direction": "request | response | hello",
  "method": "load",
  "expect": "valid | invalid",
  "message": {},
  "raw": "...",
  "expected_response": {},
  "note": "..."
}
```

- `method`: response の result 検証で必須（`<Method>Result` を選ぶ）。request では
  `message.method` と一致させる
- `message` と `raw`: どちらか一方。`raw` は JSON として解釈できない入力
- `expect`: スキーマ検証の期待。`valid` は Request / Response / HelloEvent と
  （既知メソッドなら）`<Method>Params` / `<Method>Result` に適合する
- `expected_response`: ホストが返すべき応答（Response スキーマに適合する）。省略時は
  応答しない（プロトコル違反として扱う）
- `expected_host_action`: `close_connection` は応答を返さず接続を閉じる（`id` を導出できない要求。設計 §2.2）。
  `expected_response` と排他
- `valid` でも `expected_response` がエラーのケース（`request-load-file-scheme` 等）は、
  スキーマでは表せない振る舞い（scheme 制限。設計 §2.5）を固定する

## ケースの分類

- 正常系: hello、各メソッドの要求と結果
- エラー系: `error.kind` の各値
- 未知メソッド: `unsupported`
- 未知フィールド: 応答側は無視（`valid`）、要求側は拒否（`invalid`）
- 不正入力: 欠落・型違い・範囲外・上限超過・不正 JSON

長さ接頭辞・切り詰めフレームなどバイト単位の codec の不正は JSON fixture では表せず、次の Issue のユニットテストで扱う:
Issue #989・#993。

## 追加時の手順

1. 2 スペースインデント・LF・末尾改行で `<direction>-<case>.json` を置く
2. `cargo test -p fandhe-browser-render` で契約テスト（`tests/protocol_fixtures.rs`）を通す
3. 実在のトークン・Cookie は書かない（URL は `https://example.com/` 等のダミー）
