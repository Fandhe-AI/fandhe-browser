# Security Policy

fandhe-browser の脆弱性の報告手順と対応方針を定める。正式な報告窓口は未確定で、確定後に本書へ反映する。

spec 対応: `OSS-5`（`04-behavior/oss-licensing.md`）/ TASK-79（79.4）/ MS-7。

## 対象バージョン

- リリースは未実施で、対象は `main` ブランチの最新のみ
- 開発段階であり、修正の提供を保証するサポート対象リリースは無い
- リリース開始後に本節を更新する

## 脆弱性の報告手順

1. 公開の Issue・PR・Discussions・コミットメッセージに脆弱性の詳細を書かない
2. 現時点で利用できる非公開の経路は GitHub の private vulnerability reporting。リポジトリの Security タブの「Report a vulnerability」から送る
   （<https://github.com/Fandhe-AI/fandhe-browser/security/advisories/new>）
3. 次の情報を含める
   - 対象のコミット SHA
   - OS とアーキテクチャ
   - ビルド構成（feature・JS エンジン〔V8 / boa〕・`rendering` の有無）
   - 再現手順または最小の PoC
   - 想定される影響
   - 把握していれば回避策

報告者へのお願い:

- 実在の第三者サイトや他人のデータに対して検証しない
- 実トークン・Cookie・個人情報を報告に含めない（ダミー値に置き換える）
- 修正の提供前に公開しない

## 報告窓口

正式な連絡先は未確定。TASK-79（79.h1）で確定し、本書へ反映する予定。確定までは上記の private vulnerability reporting を使う。

## 対応方針

以下は暫定の目安であり、保証（SLA）ではない。TASK-79（79.h1）の方針確定にあわせて見直す。

| 段階 | 暫定の目安 |
| ---- | ---------- |
| 受領確認 | 報告から 7 日以内 |
| 初期評価（再現可否・深刻度の一次判断）の連絡 | 受領確認から 14 日以内 |
| 進捗連絡 | 対応完了まで少なくとも 30 日ごと |
| 公開の調整 | 修正の提供後、または報告から 90 日を目安に報告者と協議 |

- 脆弱性と判断しなかった場合も理由を返信する
- 報奨金制度（bug bounty）は無い

## 対象範囲

報告を受け付けたい問題の例（防御が実装済みであることを意味しない）:

- プロファイル保管場所の外への読み書き、プロファイル間のデータ漏えい（PROF 系）
- プラグイン境界の逸脱（PLUG 系）
- ローカルの CDP 互換サーバー・AI 最適化 API サーバーへの意図しない外部到達、DNS rebinding、Origin 検査の不備（`SEC-4`。検討中のビヘイビア）
- 取得先 URL の検証不備による SSRF（`file:` 等の scheme・内部アドレス）
- 取得した HTML / JS・CDP / API リクエストによるクラッシュ・メモリ安全性の問題・無制限のリソース確保
- 秘密情報（Cookie・トークン）のログや出力への漏えい

対象外:

- UA・フィンガープリントの偽装や anti-bot 回避ができないこと（機能として提供しない方針。`SEC-2`、[anti-bot-review](./docs/design/anti-bot-review.md)・[anti-bot-exclusion](./docs/design/anti-bot-exclusion.md)）
- 利用者が loopback 以外への公開を明示的に選んだ構成そのもの、利用者環境の設定不備
- 第三者コンポーネント（V8・Servo・依存クレート）自体の脆弱性は各上流へ報告する。本リポジトリの組み込み方に起因する場合、または影響が及ぶ場合は本リポジトリへも連絡してほしい

## 開示方針

- 協調的開示（coordinated disclosure）とし、修正と公開の時期は報告者と調整する
- 公開は GitHub Security Advisories で行う。希望があれば報告者をクレジットする

## 関連ドキュメント

- [CONTRIBUTING.md](./CONTRIBUTING.md)
- [CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md)
- [.claude/rules/security.md](./.claude/rules/security.md)（開発者向けのセキュリティ規約）
