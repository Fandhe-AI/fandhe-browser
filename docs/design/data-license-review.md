# データライセンス手動レビュー記録

- **対応ビヘイビア**: `OSS-7`（関連: `OSS-2`・`OSS-3`・`OSS-4`・`PLUG-8`・`PLUG-10`）
- **対応タスク**: TASK-103（TASK-103.2・#396）。前段は TASK-103.1（#395。NOTICE への帰属表示。PR #1259）
- **マイルストーン**: MS-9
- **実施日**: 2026-10-22（UTC）
- **基準コミット**: `d93f480b`（`origin/main`）
- **状態**: 手動レビュー完了。要判断事項は末尾のとおりオーナーへ報告する。ライセンスの許可判断は Agent が行っておらず、承認は得ていない

## 目的・前提

`OSS-7` は、段階的互換プロファイル（`PLUG-8`・`PLUG-10`）が使う 3 つのデータを、Cargo の依存ではないため手動レビューで担保すると定めている。本ドキュメントはその手動レビューの記録である（`.claude/rules/licensing.md`「非 Cargo 資産」節の求める手動確認に相当）。

本書は事実の記録に留める。CC-BY-4.0 等の許可・不許可の判断は含まない。

## 対象データ

| データ | 版 | SPDX | 取得元 | リポジトリ内の所在 | 埋め込み方式 | 配布物 |
| ------ | -- | ---- | ------ | ------------------ | ------------ | ------ |
| web-features | 3.39.0（tarball sha256 は `profiles/README.md` に記録） | Apache-2.0 | npm | `profiles/chrome.json`・`profiles/safari.json`（事前生成した派生データ） | `include_str!`（`fandhe-browser-core` の `cssom_profile.rs`） | 含まれる |
| caniuse-lite | 1.0.30001810 | CC-BY-4.0 | npm | 同上 | 同上 | 含まれる |
| WPT | リビジョン `74ca910926d76710943f2a8798817102f69d6e40` | BSD-3-Clause | git（`web-platform-tests/wpt` の sparse checkout。npm ではない） | `harness/wpt_subset_runner/wpt-subset.json`（テストのパス一覧 257 件のみ） | テスト時のみ `include_str!`（`src/report.rs` の test と `tests/subset_schema.rs`） | 含まれない |

## 棚卸しの対象外である根拠

cargo-deny（`OSS-3`）と cargo-about（`OSS-4`）が走査するのは Cargo の依存グラフ（`deny.toml` の `[graph]`・`about.toml`）だけである。3 データが対象外になる根拠は次の 2 点。

1. **Rust クレート依存ではない**。`Cargo.toml` に追加しておらず、ルート `Cargo.lock` に `caniuse*`・`web-features*`・`wpt` という名前のパッケージが無い（`wpt-subset-runner` は harness の workspace member で、データではない）。リポジトリに npm の `package.json`・ロックファイルは無い（`.agents/skills/make/samples/` のスキル用サンプルを除く）。ビルド時に npm から取得する処理も無い
2. **事前生成した JSON を `include_str!` で埋め込む方式**である。ファイルの中身は Cargo のグラフに現れないため、cargo-deny / cargo-about からは見えない。したがって `deny.toml` の allow に CC-BY-4.0 等を足す必要は無く、足さない（allow は crate のライセンス判定用）

WPT の補足: テスト本体は `fetch-wpt.sh` が実行時に取得し、`.gitignore`（`wpt-work/`・`wpt/`）で除外しているため、コミットも配布もされない。パス一覧を読むのは `publish = false` の harness crate のテストだけで、リリースバイナリに入らない。

## 手動レビューの確認項目と結果

| 確認項目 | 結果 |
| -------- | ---- |
| GPL / LGPL / AGPL / MPL 系でないこと | 3 データとも Apache-2.0・CC-BY-4.0・BSD-3-Clause で、該当しない |
| 出典の記録 | `profiles/*.json` の `sources[]`（name・version・license・url）に web-features・caniuse-lite がある。WPT は `wpt-subset.json` の `source`（`wptSource`・`wptRevision`）にある |
| CC-BY-4.0 の帰属表記 | JSON の `sources[].attribution` と `profiles/README.md` が同一文言「Compatibility data derived from caniuse.com (CC-BY-4.0)」。JSON の出典表記は `tests/cssom_profile_data.rs` が検査している |
| NOTICE への記載 | PR #1259（TASK-103.1）で追加予定。本書の基準コミットでは未反映 |
| 改変の有無 | 上流データをそのまま同梱せず、`PLUG-8` 用に feature を絞って再構成した派生データ。経緯は `profiles/README.md`「生成手順」に記録がある |
| 配布物への混入 | プロファイル JSON は全ビルドのバイナリに入る。WPT のテスト本体は入らない |

## 証跡（再現手順）

基準コミットで実行し、ネットワーク取得はしていない。

```bash
grep -nE 'name = "(caniuse|web-features|wpt)' Cargo.lock           # wpt-subset-runner の 1 件のみ
git ls-files | grep -E '(^|/)package(-lock)?\.json$|pnpm-lock|yarn.lock'  # スキルのサンプルのみ
git grep -n 'include_str!' -- crates harness | grep -E 'profiles/|wpt-subset'
jq -c '.sources' profiles/chrome.json profiles/safari.json
jq '.source.wptRevision' harness/wpt_subset_runner/wpt-subset.json
```

## 再レビューのトリガー

- web-features・caniuse-lite の版の更新、`profiles/*.json` の再生成
- `wpt-subset.json` の WPT リビジョンの更新
- Cargo 外のデータを新たに `include_str!` 等で埋め込むとき
- npm のマニフェスト・ロックファイルを導入するとき
- WPT のテスト本体を同梱する方針に変えるとき

## 要判断事項

オーナーの判断が必要。Agent は決めておらず、承認は得ていない。

1. spec の `OSS-7`・TASK-103 は 3 データを「いずれも npm パッケージ」としているが、WPT は npm ではなく git から取得している。spec の文言を直すかは spec リポ側の判断
2. NOTICE の帰属表示（#395・PR #1259）がマージされたら、本書の「NOTICE への記載」を実態に合わせて更新する
