# Servo ホストの egress 隔離方式: 3 OS の比較設計と実測計画

- **状態**: 設計（比較と実測計画）。**方式の最終決定は実測の後にオーナーが行う**（#1003）。2026-10-31 の暫定判断（Linux を先行・macOS と Windows は実測まで D の扱い）を §8 に、Linux の実測結果を §6.4 に記録した
- **対応**: TASK-38・MS-4・`RENDER-3`・`SEC-2`・`PLUG-1`・`XOS-1`・Issue #1368（親判断 #1003）。関連は `RENDER-1`。docker 経路は #1386 で再実測した（§6.4）。後続は #1387（macOS・Windows の実測ハーネス）
- **入力資料**: [servo-host-egress-options.md](./servo-host-egress-options.md)（#1317。手段ごとの詳細表・Servo 0.7 の net 層の事実・承認事項の洗い出し。以下「options §n」と書く）。設計の親は [servo-host-design.md](./servo-host-design.md) §2.5・§13 項目 6
- **確度の表記**: 「確認済み」は原文またはコードを確認した事項、「推測」は読んだ範囲からの推論、「未確認」は調べていない事項。一次情報は options の出典（参照日 2026-10-29、Servo は `v0.7.0` タグ）に従い、判断の前に原文で再確認する。本書では新たな外部ページの再取得はしていない
- **spec 側の課題**（本リポからは編集しない。要報告）: egress 方針に専用のビヘイビア ID が無い。`RENDER-3` の本文はホストのサイズ・ビルド時間とオプトイン配布の設計材料で egress を扱わない。対応づけの確認は spec 側で行う

## 1. 2026-10-31 のオーナー判断の枠組み（#1003）

- **B（OS のネットワーク隔離）を本命**にする
- **A（Linux の `egress_guard.py` 型の事後検知）は補助**にする。A 単独は採らない（事後検出で SSRF の副作用を止められない。Linux 専用で `XOS-1` に合わない）
- B が成立しない OS は、既知の制約として利用者向け文書に明記する（D との併用）
- 方式の最終決定は実測の後。本書は最初の作業である比較設計と実測計画
- core が取得済みの HTML を渡す方式（#1003 の選択肢 3）は、2026-10-21 の判断で最後の手段とされており、比較の対象外

B の具体形は次の 2 つ。外部 http/https に出る要件があるため、現実的なのは後者（根拠は options §4）。

| 形 | 内容 |
| -- | ---- |
| 全遮断 | ホストのネットワークを丸ごと切る。保存済みページの撮影（`harness/render-screenshot/isolation_check.py` の `docker run --network none`。#1281）には使える前例がある。外部に出る用途とは両立しない |
| プロキシ＋接続先制限 | 本体側の許可リスト付きプロキシに Servo の HTTP(S) を通し、OS 隔離で「プロキシ以外へ出られない」ようにする |

## 2. 覆うべき経路と役割分担

| 経路 | プロキシ pref を通るか | 備考 |
| ---- | ---------------------- | ---- |
| HTTP リダイレクト・サブリソース（img・CSS・script・iframe・fetch / XHR） | 通る見込み（推測。options §2） | 1 本ごとに CONNECT になるため許可リストが毎回効く。プロキシが見るのは `host:port` まで |
| WebSocket | 通らない（確認済み。`websocket_loader.rs` が `TcpStream::connect` で直接接続） | OS 隔離でしか塞げない |
| DNS（ローカルでの名前解決・リバインディング） | プロキシ経由ならホスト名は CONNECT で渡る（推測） | プロキシ未使用の経路は `HyperHttpConnector` がローカルで引く（確認済み） |
| UDP（QUIC・直接 DNS 等） | 通らない | OS 隔離の対象 |
| その他の直接接続 | 不明 | net 層の網羅は未確認（options §7 の 1） |

役割分担の方針（設計案であり決定ではない）:

- 宛先 IP の判定（内部アドレスの遮断）は本体側プロキシが接続時に再検査して担う。DNS リバインディングへの実効的な対策もここ
- OS 隔離は「プロキシ以外へ出られない」ことを担う。OS 手段の多くは宛先 IP を絞れないため
- `allow_private_network_access`（既定 false）を弱める案は書かない。`true` 指定の意味は隔離を入れても変えない
- 隔離・プロキシ構成によって UA 等の識別面を変えない（`SEC-2`）
- 本体側プロキシは loopback のみに bind し、認証なしで外部へ公開しない（`security.md`）

## 3. 3 OS の比較表

観点は次の 6 つ。各セルの末尾に確度を付ける。手段ごとの長い記述と一次情報の URL は options §3 を参照する。

- ① 公開アドレスだけを許し内部アドレスを遮断できるか（宛先の粒度）
- ② リダイレクト・サブリソース・WebSocket・DNS の被覆
- ③ 必要な権限
- ④ 配布形態との整合。Servo ホストは musl 静的・scratch では配布せず、glibc（Debian slim）系の別イメージ（設計 §13 項目 8）。musl 静的・scratch は本体の既定・軽量コンテナだけに当てはまる
- ⑤ `allow_private_network_access` との整合
- ⑥ `PLUG-1` との関係（外部バイナリ・FFI・`unsafe`・動的ロードを増やさないか）

### 3.1 Linux

| 手段 | ① 宛先の粒度 | ② 被覆 | ③ 権限 | ④ 配布 | ⑤ PNA | ⑥ PLUG-1 |
| ---- | ------------ | ------ | ------ | ------ | ----- | -------- |
| netns（user ns + net ns。`unshare` / `bwrap`） | 全遮断。プロキシへは中継プロセスが要る（options §3.1。推測） | 全経路（WebSocket・UDP・DNS 含む） | カーネル上は非特権。Ubuntu 24.04 の AppArmor 制限で ns 内 capability が拒否される（options の記述）。カーネル 7.0.0-34 の Ubuntu では `unshare -n` が EPERM、`bwrap --unshare-net` は成功（確認済み。§6.4） | Docker 既定 seccomp 下で `unshare -n` は EPERM（確認済み。§6.4）。イメージに bwrap が無く、Docker 内での bwrap の可否は未確認 | 変えない | `pre_exec` + `unshare` は `unsafe`。`bwrap` は外部バイナリ |
| コンテナ外部の netns（`docker run --network none` 等） | 全遮断。プロキシへは別途経路が要る（未設計） | 全経路 | docker 権限（ホスト側の運用） | 前例あり（`isolation_check.py`。#55 の判断。保存済みページ撮影のみ） | 変えない | 本体の外側の隔離で、ホスト本体のコードは増えない |
| Landlock net（ABI 4、6.7 以降） | TCP のポート単位。宛先 IP は不可（options §3.1。確認済みとして記載） | TCP は被覆。ABI 4 は UDP・DNS を制限しない | 非特権（`no_new_privs`）。userns 不要 | Docker 既定 seccomp は `landlock_*` を許可（options の記述） | プロキシと同ポートの別 IP への直接接続は塞げない | 依存（`landlock` 等）と syscall の `unsafe` が候補（承認事項） |
| seccomp-bpf（socket 種別） | `socket()` の domain・type のみ。`connect()` の宛先は見えない | UDP・RAW を禁止して補完する役 | 非特権 | 同上 | 宛先 IP は絞れない | BPF 組み立ての依存か自前（承認事項） |
| cgroup BPF / nftables | IP・ポート | 全経路 | root / `CAP_NET_ADMIN` / `CAP_BPF` | 非特権起動の前提と衝突 | 絞れる | 要件に合わない（推測） |
| 補助 A: `egress_guard.py` | 検出のみ（接続済みソケットの宛先） | 25 ms 未満の短命な接続は取りこぼす。別 netns は検査不能 | 非特権 | Linux 専用 | 検出のみ | リポ内スクリプトで追加なし |

### 3.2 macOS

| 手段 | ① 宛先の粒度 | ② 被覆 | ③ 権限 | ④ 配布 | ⑤ PNA | ⑥ PLUG-1 |
| ---- | ------------ | ------ | ------ | ------ | ----- | -------- |
| `sandbox-exec -p`（Seatbelt） | 全遮断、`localhost:<port>`、`*:<port>`、プロトコル別。ホスト名・任意 IP は不可（Bazel #5206 の議論。Apple 公式ではない） | プロファイルでソケット全般を拒否できる見込み（未実測） | 非特権 | OS 同梱。**man に DEPRECATED とある**（options の記述） | 宛先 IP は絞れない | `Command` で外部の `/usr/bin/sandbox-exec` を起動するだけ。`unsafe`・依存は不要。別プロセス境界のみの方針と矛盾しないか要確認 |
| `sandbox_init()` | 同上 | 同上 | 非特権 | 非推奨の有無は未確認 | 同上 | C FFI の `unsafe` |
| App Sandbox（`network.client`） | 真偽値のみ | — | 署名済みバンドルが前提 | CLI の素の実行ファイルに効くか未検証 | — | — |
| Network Extension / PF | フローごと / IP・ポート | — | システム拡張・root | 要件に合わない（一般知識。推測） | — | — |

### 3.3 Windows

| 手段 | ① 宛先の粒度 | ② 被覆 | ③ 権限 | ④ 配布 | ⑤ PNA | ⑥ PLUG-1 |
| ---- | ------------ | ------ | ------ | ------ | ----- | -------- |
| AppContainer（capability なし） | 全遮断。loopback は既定で通らない | 全経路 | 作成は非特権。loopback 例外（`CheckNetIsolation` / `NetworkIsolationSetAppContainerConfig`）は開発・デバッグ用途とされ、管理者要否は未確認 | Servo ホスト（ANGLE の DLL・フォント・一時ディレクトリ）が中で動くか未検証 | プロキシ構成にするには loopback 例外が鍵 | `CreateProcess` の属性リストで `windows-sys` と `unsafe`（承認事項） |
| AppContainer + `internetClient` | インターネット宛を通し、LAN・loopback は通さない設計（Unity 資料のみ。推測） | 公開ホスト宛は止まらず、許可リストにならない | 非特権 | 同上 | private / loopback 宛だけを OS で落とす用途に限り有効かもしれない | 同上 |
| WFP / ファイアウォール規則 | IP・ポート・アプリ | 全経路 | 管理者（実質）。WFP の非特権フィルタ追加の可否は未検証 | 要件に合わない | 絞れる | — |
| Job Object | ネットワーク制限クラスなし（記憶。未確認） | — | — | — | — | — |

## 4. OS ごとの暫定見立て（採否ではない）

| OS | 見立て | 決める未確認事項 |
| -- | ------ | ---------------- |
| Linux | 見込みあり（条件付き）。Landlock + seccomp + プロキシ、またはコンテナ外部の netns。後者は運用側で隔離する前提。2026-10-31 の暫定判断で bwrap `--unshare-net` を主方式とした（§8） | 6.7 未満のカーネルでの扱い。宛先 IP を絞れない限界（options §4.1）。Docker 既定 seccomp と Ubuntu の AppArmor 制限下の netns の作り方は §6.4 で確認済み（Docker 内での bwrap の可否は未確認） |
| macOS | 条件付き。`(deny network*)` + loopback のプロキシのポートだけ許可 | `remote ip "localhost:<port>"` の実動作。非推奨 API が将来の OS で消える可能性。ヘッドレスでの GPU・フォントの動作（`RENDER-4` の前歴） |
| Windows | 不成立の公算が高い | AppContainer の loopback 例外に管理者権限が要るか。ホストが AppContainer 内で動くか |

B が成立しない OS の既知の制約として明記する文言（D）の置き場所の候補は、設計 §2.5 と README。文言の方針は §8 の 2（2026-10-31 の暫定判断）で決まった。置き場所は未定。

## 5. 補助 A の位置づけ

- Linux の実測での観測手段として使う。B を入れた後の多層防御としても使える
- 限界を再掲する: 25 ms ポーリング・別 netns のプロセスは検査不能（`GuardUnavailable`）・Linux 専用。A の結果だけを保証として扱わない

## 6. 実測計画

### 6.1 共通の観測方法

loopback で配信するフィクスチャページ（`harness/render-screenshot/serve_pages.py` の流用を前提とする）で、実在の外部サイトや内部ネットワークには向けない。ページは次を試みる。

- (i) 許可外ホストへのリダイレクト
- (ii) 許可外ホストのサブリソース（img・fetch）
- (iii) 許可外の宛先への `new WebSocket()`
- (iv) 内部アドレス（プライベート IP・link-local）への接続
- (v) DNS・UDP の直接送信

合否の観測:

- 合格は、許可リスト外への接続が 0 件と観測されること。観測できない場合は fail-closed（不合格）
- 陽性の確認も行う: 隔離の下でも、ホストの `--selftest` と loopback ページの `load` が成功すること
- 観測手段は、プロキシのアクセスログ、許可外の宛先に置く受信側 listener（loopback または別 netns のダミー）、`isolation_check.py` 型の connect だけのプローブ（何も送らない）。Linux ではさらに `egress_guard.py` を併用する
- 実測ログには URL のクエリ・本文・プロキシ系の環境変数の値を載せず、有無だけを記録する。`sandbox-exec -p` のプロファイル等は固定値で、外部入力を連結しない

### 6.2 OS ごとの計画

| OS | 測ること | 環境 | 既知の限界 |
| -- | -------- | ---- | ---------- |
| Linux | Landlock ABI（`landlock_create_ruleset` のバージョン問い合わせ）。Landlock + seccomp 適用後のホストの動作。UDP・DNS の漏れ。`docker run`（Debian slim）入れ子での既定 seccomp 下の挙動。`--network none` の前例との比較 | ローカル + `xos-measure.yml` の ubuntu runner（sudo あり・Docker の中ではない）。6.7 未満はローカル VM または別ディストリ | runner のカーネルが固定される |
| macOS | `sandbox-exec` の `(deny network*)` + プロキシのポートのみ許可で、WebSocket の直接接続が止まるか。ホストが隔離下で動くか。OS 版による差（runner のイメージ版を記録） | `xos-measure.yml` の macos runner（ローカルでも可） | `--selftest` が隔離と無関係に失敗する場合の切り分け手順を先に用意する（ヘッドレスの GPU・WindowServer 制限。`RENDER-4`） |
| Windows | AppContainer の loopback 例外に管理者が要るか。プロキシ宛だけを通せるか。ANGLE の DLL・フォント・一時ディレクトリを含めホストが AppContainer 内で起動するか | **非管理者の実機またはローカル VM が必要**。`xos-measure.yml` の windows runner は管理者で動くため、鍵となる問いには答えられない | runner だけでは権限の問いに結論が出ない |

Windows の補足（#1401）: `xos-measure.yml` の Windows ジョブに限り、opt-in のループバック免除（`CheckNetIsolation LoopbackExempt`。ハーネスが作った AppContainer の SID だけ・計測後に解除。オーナー承認 2026-11-01）をつけた preflight を追加した。結果は #1003 に記録する。

### 6.3 実施の扱い

- `xos-measure.yml`（手動専用・required ではない）への suite 追加と測定ハーネスの実装は後続 issue（infra-builder・render-builder）とする。本 issue では計画を書くだけにする。Linux 版のハーネスは `harness/egress-isolation/`（#1377・#1380）で実装済み（結果は §6.4）。macOS・Windows 版は #1387 で同ハーネスを拡張し、`xos-measure.yml` の suite `egress-isolation` として実装した（macOS は Seatbelt の `seatbelt`、Windows は `baseline` と AppContainer。AppContainer のランチャーは #1396 でオーナー承認〔2026-11-01。`harness/egress-isolation/` の Python 計測ハーネスに限る ctypes の Win32 FFI。本体 crate の FFI・`unsafe` は対象外〕のうえ実装し、loopback 遮断と egress 遮断の切り分けは Servo を使わない preflight で行う。結果は #1003 に記録し、実測後に本書へ要約する）。macOS の TCP 経路の試行は、拒否ログだけでは観測できなかったため #1395 で失敗経路のビーコン（許可オリジンへの GET。補助情報のみで試行の根拠にしない。試行の根拠は UDP の errno）と拒否ログの診断・自己検査を加えた（手段の詳細は `harness/egress-isolation/README.md`）。TCP が観測不能だった原因は、macOS 26 の拒否行の形式（`deny(1) network-outbound <word>:*:<port>`。宛先アドレスが `*`）が想定の IPv4 形式と異なっていたことだけだった。#1409 でこれを追加形式として受理し、帰属は「ホストの pid」と「その run の一時 listener ポート」の両方一致に限る（アドレス・プロトコルは検証せず、ポートが重複する場合は帰属しない）。拒否ログの収集は sudo なし（非特権）の `log show` で足りる。sudo つき `log show --info --debug`・`log config` によるログレベル変更・`--seatbelt-sudo-log` の opt-in は #1402 で一時導入したが、実測（xos-measure run 41309744793・41310673642・41313641135）で非特権の `log show` でも拒否行が同じだけ取れ、TCP 4 経路（img・fetch・ws・internal）と UDP の udp_direct が pass・自己検査が成立したため、オーナー判断（2026-11-01）により #1414 で削除した。redirect は試行が観測できず unobservable のまま。ランナー設定の `sudo ifconfig lo0 alias 127.0.0.2/127.0.0.3` は残る（結果は #1003）
- 依存・`unsafe` を要する測定用コードは承認の後にする

### 6.4 Linux の実測結果（c56f11c2・2026-10-31）

`harness/egress-isolation/`（#1377・#1380）で Linux を 1 回測った結果の要約（TASK-38・`RENDER-1`）。依存・`unsafe` を使わない範囲の測定で、値は #1003 のコメント「Linux の egress 隔離の実測（c56f11c2・2026-10-31）」の範囲に限る。生データと根拠の詳細はそちらを正とする。測定は事実の記録で、方式の採否はここで決めない（判断は §8）。

環境:

| 項目 | 値 |
| ---- | -- |
| コミット | `c56f11c2`（origin/main） |
| OS・カーネル | Ubuntu・カーネル 7.0.0-34-generic（x86_64・glibc 2.43） |
| sysctl | `kernel.apparmor_restrict_unprivileged_userns=1`・`kernel.unprivileged_userns_clone=1`・`user.max_user_namespaces=102024`・`net.ipv4.ip_unprivileged_port_start=1024` |
| ツール | bubblewrap 0.11.1・Docker 29.8.1 |
| Landlock | LSM に有効・ABI 8（バージョンの問い合わせのみ。ルールの適用は未測定） |
| netns の作り方 | `bwrap --unshare-net` は成功。特権なしの `unshare -n`・`unshare -rn` は EPERM（AppArmor の制限下）。bwrap の ns 内には `CAP_NET_ADMIN` が無く、`ip addr add` は EPERM（veth は作れない） |
| Docker 既定 seccomp 下 | `unshare -n` は EPERM、Landlock ABI の問い合わせは成功、イメージに bwrap は無い（a5770058 時点の測定。#1003 のコメント「Linux 実測の記録（#1377 のハーネス）」） |
| ホスト | `servo-host` の dev ビルド（`cargo build --manifest-path servo-host/Cargo.toml --locked`）。プロキシ系環境変数なし |

モード別の結果（経路の記号は §6.1）:

| 経路 | ベースラインの外側受信（陽性対照） | bwrap `--unshare-net` | docker `--network none` | 観測手段 |
| ---- | ---------------------------------- | --------------------- | ----------------------- | -------- |
| redirect（i） | 1 | pass | pass | 外側・内側の listener |
| img（ii） | 1 | pass | pass | 同上 |
| fetch（ii） | 1 | pass | pass | 同上 |
| ws（iii） | 1 | pass | pass | 同上 |
| internal（iv。代替宛先 127.0.0.3） | 1 | pass | pass | 同上 |
| udp_direct（v） | 1 | pass | pass | ハーネスの UDP プローブ（Servo の送信ではない） |
| internal_real（iv。`10.255.255.1`・`169.254.169.254`・`fd00::1`） | 対象外 | pass（connect が `ENETUNREACH`） | pass（構造検査） | 経路表と connect のみの構造検査（外部からの観測ではない） |
| dns（v） | 対象外 | pass（固定 nameserver への UDP connect が `ENETUNREACH`・`getaddrinfo` は `gaierror:-3`） | pass（構造検査） | 経路表とハーネスの `getaddrinfo` による構造検査（Servo 自身の DNS は観測していない） |

- bwrap `--unshare-net`: 8 経路すべて pass。全経路で外側 listener の受信 0 件・内側 listener の試行 1 件。`egress_guard` は 856 回走査して違反なし（補助の位置づけ）。隔離の事前検査は「lo のみ」、ホストの sha256 はベースラインと一致した
- docker `--network none`（#1386。2026-10-31 に再実測）: 8 経路すべて pass。配布コンテナと同じ Debian 13（glibc 2.41）のイメージ内でホストを release ビルドして取り出し（`harness/egress-isolation/container/prepare.sh`）、baseline・bwrap・docker の全モードに同一バイナリ（sha256 889cb467…865f4・GLIBC の最大要求 2.39）を使った。イメージ ID は sha256:cded5685…89e8。全経路で外側 listener の受信 0 件・内側 listener の試行 1 件、隔離の事前検査は「lo のみ」、`egress_guard` は 941 回走査して違反なし。測定値は #1003 のコメントに記録した
- 正の対照: bwrap と docker（上記の再実測）のどちらでも `--selftest` は rc=0、loopback ページの `load` は成功した。`redirect` のページの `load` は baseline を含む全モードで timeout で、隔離に起因する差ではない（外側の受信は baseline で 1 件）
- 先行測定（参考。再実測で置き換え済み）: a5770058 時点の測定（#1003 のコメント「Linux 実測の記録（#1377 のハーネス）」）では、2026-10-27 ビルドのイメージから取り出したホストをベースラインと隔離の両方に使い、docker `--network none` でも外側の受信は 0 件だった。ホストが現在のコミットのビルドではないため、本節は c56f11c2 の測定を記録とする

未測定（残りの追跡は §9.2）:

- Servo 自身の DNS・UDP 送信の外部観測（root か `CAP_NET_ADMIN` が要る）
- Landlock ルールの適用・seccomp・許可リスト付きプロキシとの併用（§7 の承認事項）
- macOS・Windows（#1387）、AppArmor の制限が無い環境・古いカーネル

## 7. 承認事項（実装しない）

方針は options §5 を参照。builder は着手しない（`dependency-policy.md`・`coding-rust.md`）。

| 項目 | 内容 |
| ---- | ---- |
| 依存 | `landlock`、seccomp 用クレート、`windows-sys`（いずれも未選定。ホスト側は `servo-host/` の別 workspace に置く案） |
| `unsafe` | `pre_exec`、`sandbox_init` の FFI、`CreateProcess` の属性リスト（計測ハーネス `harness/egress-isolation/appcontainer.py` の ctypes は #1396 で承認・実装済み。本体 crate は対象外） |
| 新設 | 本体側の許可リスト付きプロキシ（CONNECT。loopback 限定 bind・接続時の IP 再検査） |
| 変更 | `servo-host/src/engine.rs` の `fixed_preferences`（proxy pref の注入。クライアントから任意引数を渡さない方針〔設計 §2.6〕と整合させる） |
| CI | `xos-measure.yml` への suite 追加（required ではないため ruleset への影響なし。`egress-isolation` は #1387 の指定で追加済み） |

## 8. オーナー判断（2026-10-31・暫定）

- **判断日**: 2026-10-31。根拠は #1003 のコメント「オーナー判断（2026-10-31）: §8 の 3 点」で、Linux の実測（§6.4）を受けたもの
- **macOS・Windows の実測前の暫定判断**で、実測の後に見直す。方式の最終決定（§1）は引き続き実測の後に行う
- 対応: TASK-38・`RENDER-1`・`SEC-2`・`XOS-1`

| 論点（判断前の問い） | 判断 | 後続 |
| -------------------- | ---- | ---- |
| 1. GA で必須とする成立範囲（3 OS 揃えるか、Linux を先行するか） | Linux を先行する。Linux は B（OS 隔離）として bwrap `--unshare-net` を主方式にする（§6.4 で 8 経路すべて pass）。docker `--network none` は、配布コンテナとホストの glibc をそろえてから測り直す | #1386（測定済み。§6.4・§9.1） |
| 2. B が成立しない OS・環境での既知の制約の文言（D） | OS 隔離が使えない環境では A（アプリ側の検知）だけで動作し、「egress を完全には遮断できない」と明記する。既定ではそのことを警告する | 文言の置き場所は §4 の候補（設計 §2.5・README）から選ぶ（未定） |
| 3. Windows の扱い（弱い保証にとどめるか、対象外とするか） | macOS・Windows の実測が済むまで GA の成立範囲から外し、D の扱いとする | #1387 |

- 「8 経路すべて pass」のうち `internal_real`・`dns` は構造検査で、Servo 自身の DNS・UDP 送信は外部から観測していない（§6.4）

## 9. 未確認事項

options §7 を引き継ぐ。本書で新たに加わる未確認事項はない。§6.2 で確かめるとしていた Docker 既定 seccomp 下での `unshare` の挙動と Landlock の実機の対応は、Linux（カーネル 7.0.0-34）について §6.4 で確かめた。

### 9.1 2026-10-31 の Linux 実測で解消した事項

詳細は §6.4（TASK-38・`RENDER-1`）。

- Docker 既定 seccomp 下の `unshare -n`: EPERM
- Ubuntu の AppArmor 制限（`kernel.apparmor_restrict_unprivileged_userns=1`）下での netns の作り方: 特権なしの `unshare -n`・`unshare -rn` は EPERM、`bwrap --unshare-net` は成功。ns 内に `CAP_NET_ADMIN` は無い
- Landlock の対応: カーネル 7.0.0-34 で LSM に有効・ABI 8（問い合わせのみ）
- 現在のコミットのホストでの再測定: c56f11c2 の dev ビルドで測り、bwrap `--unshare-net` は 8 経路すべて pass
- docker `--network none` の判定（配布コンテナとホストの glibc をそろえた同一バイナリ。#1386）: 8 経路すべて pass（§6.4）

### 9.2 残る未確認事項と追跡先

| 未確認事項 | 追跡先 |
| ---------- | ------ |
| macOS・Windows での成立性（§3.2・§3.3・§6.2） | #1387 |
| Docker 内での bwrap の可否（イメージに bwrap が無い） | 未起票 |
| Servo 自身の DNS・UDP 送信の外部観測（root か `CAP_NET_ADMIN` が要る） | 未起票 |
| Landlock ルール適用下の動作・seccomp・許可リスト付きプロキシとの併用 | §7 の承認事項 |
| AppArmor の制限が無い環境・6.7 未満のカーネル | 未起票 |
| net 層の直接接続の網羅 | options §7 の 1 |
