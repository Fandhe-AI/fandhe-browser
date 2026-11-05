# Servo ホストの egress 隔離方式: 3 OS の比較設計と実測計画

- **状態**: 設計（比較と実測計画）。**方式の最終決定は実測の後にオーナーが行う**（#1003）。2026-11-02 の見直し（GA の成立範囲は Linux と macOS の B・Windows は D）を §8.1 に、2026-10-31 の暫定判断（履歴）を §8.2 に、Linux の実測結果を §6.4 に記録した
- **対応**: TASK-38・MS-4・`RENDER-3`・`SEC-2`・`PLUG-1`・`XOS-1`・Issue #1368（親判断 #1003）。関連は `RENDER-1`・#1458（テストの検出力と残余リスク欄）。docker 経路は #1386 で再実測した（§6.4）。後続は #1387（macOS・Windows の実測ハーネス）。macOS のプロキシの UNIX ソケット化の調査は §10（#1486）。残課題の扱いは 2026-11-04 のオーナー判断（#1457）で決まり、§9.5 に記録した（#1488）
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
| netns（user ns + net ns。`unshare` / `bwrap`） | 全遮断。プロキシへは中継プロセスが要る（options §3.1。推測） | 全経路（WebSocket・UDP・DNS 含む） | カーネル上は非特権。Ubuntu 24.04 の AppArmor 制限で ns 内 capability が拒否される（options の記述）。カーネル 7.0.0-34 の Ubuntu では `unshare -n` が EPERM、`bwrap --unshare-net` は成功（確認済み。§6.4） | Docker 既定 seccomp 下で `unshare -n` は EPERM（確認済み。§6.4）。イメージに bwrap が無い。配布用コンテナには bwrap を入れない（§9.5・#1457） | 変えない | `pre_exec` + `unshare` は `unsafe`。`bwrap` は外部バイナリ |
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

B が成立しない OS の既知の制約として明記する文言（D）の方針は §8.2 の 2（2026-10-31）で決まり、置き場所は §8.1 の 2（2026-11-02）で README「実装方針（要点）」と設計 §2.5 に決まった。

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

- `xos-measure.yml`（手動専用・required ではない）への suite 追加と測定ハーネスの実装は後続 issue（infra-builder・render-builder）とする。本 issue では計画を書くだけにする。Linux 版のハーネスは `harness/egress-isolation/`（#1377・#1380）で実装済み（結果は §6.4）。Linux の netns 内の中継から UDS 経由でプロキシへ届くかは #1431 で実測した（§6.5）。macOS・Windows 版は #1387 で同ハーネスを拡張し、`xos-measure.yml` の suite `egress-isolation` として実装した（macOS は Seatbelt の `seatbelt`、Windows は `baseline` と AppContainer。AppContainer のランチャーは #1396 でオーナー承認〔2026-11-01。`harness/egress-isolation/` の Python 計測ハーネスに限る ctypes の Win32 FFI。本体 crate の FFI・`unsafe` は対象外〕のうえ実装し、loopback 遮断と egress 遮断の切り分けは Servo を使わない preflight で行う。結果は #1003 に記録し、実測後に本書へ要約する）。macOS の TCP 経路の試行は、拒否ログだけでは観測できなかったため #1395 で失敗経路のビーコン（許可オリジンへの GET。補助情報のみで試行の根拠にしない。試行の根拠は UDP の errno）と拒否ログの診断・自己検査を加えた（手段の詳細は `harness/egress-isolation/README.md`）。TCP が観測不能だった原因は、macOS 26 の拒否行の形式（`deny(1) network-outbound <word>:*:<port>`。宛先アドレスが `*`）が想定の IPv4 形式と異なっていたことだけだった。#1409 でこれを追加形式として受理し、帰属は「ホストの pid」と「その run の一時 listener ポート」の両方一致に限る（アドレス・プロトコルは検証せず、ポートが重複する場合は帰属しない）。拒否ログの収集は sudo なし（非特権）の `log show` で足りる。sudo つき `log show --info --debug`・`log config` によるログレベル変更・`--seatbelt-sudo-log` の opt-in は #1402 で一時導入したが、実測（xos-measure run 41309744793・41310673642・41313641135）で非特権の `log show` でも拒否行が同じだけ取れ、TCP 4 経路（img・fetch・ws・internal）と UDP の udp_direct が pass・自己検査が成立したため、オーナー判断（2026-11-01）により #1414 で削除した。redirect は、#1415 の予熱（既定 5 回。判定・集計には使わない）を入れた macOS 26 の実測で pass になった（run 41401095620・41402439545。判定基準は変えていない。詳細は `harness/egress-isolation/README.md` の予熱の節）。ランナー設定の `sudo ifconfig lo0 alias 127.0.0.2/127.0.0.3` は残る（結果は #1003）
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

- Servo 自身の DNS・UDP 送信の外部観測（root か `CAP_NET_ADMIN` が要る）。#1461 で xos-measure の `egress-host-ubuntu` に host-capture（tcpdump とサンプラーによる帰属判定）を追加した。実測は run 41650422295（main）・41703923217（#1474 のブランチ。main の実測ではない）で verdict pass（記録は #1457。§9.5）。帰属できない DNS・UDP は記録だけで合否に使わず（案 A・#1457）、Servo の観測区間内の件数と宛先の上位 10 件を Summary と `verdict.json` に出す（#1467）。キャプチャの終端はサンプラーの最終カナリア（`end` 記録の `final_canary_t` 以降に観測されたサンプラー帰属のカナリア）で確かめ、観測されなければ観測不能とする（#1470 の回帰。最後のパケットの時刻との比較は廃止）。件数を数える区間には両端 0.5 秒の余裕を足し（陽性対照と終端の閾値には足さない）、stop ステップの固定の待ちは pcap に最終カナリアが現れるまで上限つきでポーリングする方式に置き換えた（#1473）。`servo_unisolated` には `render3_egress_required_blocks_direct_websocket` が意図して起動する隔離なしの対照ホストが現れうるが、判定は fail にしない
- Landlock ルールの適用・seccomp・許可リスト付きプロキシとの併用（§7 の承認事項。保留と決めた。§9.5）
- macOS・Windows（#1387）
- AppArmor の制限が無い環境・古いカーネル: 測る仕組みは入れた（xos-measure の `egress-host-ubuntu-noaa`。#1487・PR #1491）が、実測と転記はまだ。6.7 未満のカーネルはホステッドランナーでは測れず未検証（§9.5）

### 6.5 Linux の proxy-relay 実測結果（#1431・2026-11-02）

§8.1 の方式 B（bwrap `--unshare-net` ＋ 許可リスト付き CONNECT プロキシ）で必要になる「netns 内の中継 → UDS → 親のプロキシ」の構成が成り立つかを、Servo を使わずに `harness/egress-isolation/relay_probe.py` で 1 回測った結果（TASK-38・`RENDER-3`・`SEC-2`・`XOS-1`）。依存・`unsafe` は使わない。手順・判定は `harness/egress-isolation/README.md` の proxy-relay の節を正とする。

| 項目 | 値 |
| ---- | -- |
| コミット | `06d4ba30`（origin/main） |
| カーネル・bwrap | 7.0.0-34-generic（x86_64）・bubblewrap 0.11.1 |
| AppArmor | `kernel.apparmor_restrict_unprivileged_userns=1`（制限あり。bwrap 経由では netns を作れた） |

| 項目 | 結果 | 観測 |
| ---- | ---- | ---- |
| `lo`（netns 内の loopback） | pass | 127.0.0.1 の bind・listen・往復に成功 |
| `uds`（`--bind` / `--ro-bind` した UDS への connect） | pass | rw・ro の両方に connect でき、プロキシが 2 接続を数えた |
| `relay`（中継経由で外側 listener へ。陽性対照） | pass | 200 応答・nonce の往復が一致・外側の受信 1 件 |
| `direct`（中継なしの直接 connect。陰性対照） | pass | `ECONNREFUSED`。ベースラインでは届き、プロキシ経由でない受信は 0 件 |
| `ip_recheck`（許可リストに載った内部 IP への CONNECT） | pass | 403（`ip_denied`）。内部 listener の受信 0 件（ベースラインでは 1 件） |

- プロキシのポリシーは測定用のフィクスチャで、本番のものではない。本番は loopback を含む内部アドレスをすべて拒否するはずで、ここでは 127.0.0.4 を許可先の代役にしている。
- `direct` は「netns 内の `127.x` は netns 自身の `lo` に向かう」性質に頼る陰性対照で、外へ出る経路の遮断そのものは §6.4 の実測が担う。
- #1433 で bwrap の FS を `--ro-bind / /` に絞り（書き込めるのは `/run` の tmpfs・UDS ディレクトリ・出力ディレクトリだけ）、同じ構成で 5 項目がすべて pass することを再確認した。（測定は `--ro-bind / /` だが、本番（#1436・PR #1446）は `--dev-bind / /` のため、測定は FS 上の UDS の迂回を覆っていない。本番は #1485 で `/run`・`/tmp`・`$HOME` を、#1492 で `/var/tmp`・`/dev/shm`・WorkDir の親も tmpfs で隠すようにしたが、この測定の構成そのものは測り直していない。§9.3・§9.4 を参照。#1458）
- 結論: この環境では構成が成り立つ。本番のプロキシの実装・`servo-host` への注入・HostProcess への bwrap の組み込みは §7 の承認事項のまま。
- 未測定: 名前解決後の再検査（DNS rebinding）、CONNECT 上の TLS、Servo 0.7.0 の `Tunnel` との実接続、AppArmor の制限が無い環境・6.7 未満のカーネル。

## 7. 承認事項（実装しない）

方針は options §5 を参照。builder は着手しない（`dependency-policy.md`・`coding-rust.md`）。

| 項目 | 内容 |
| ---- | ---- |
| 依存 | `landlock`、seccomp 用クレート、`windows-sys`（いずれも未選定。ホスト側は `servo-host/` の別 workspace に置く案） |
| `unsafe` | `pre_exec`、`sandbox_init` の FFI、`CreateProcess` の属性リスト（計測ハーネス `harness/egress-isolation/appcontainer.py` の ctypes は #1396 で承認・実装済み。本体 crate は対象外） |
| 新設 | 本体側の許可リスト付きプロキシ（CONNECT。loopback 限定 bind・接続時の IP 再検査） |
| 変更 | `servo-host/src/engine.rs` の `fixed_preferences`（proxy pref の注入。クライアントから任意引数を渡さない方針〔設計 §2.6〕と整合させる） |
| CI | `xos-measure.yml` への suite 追加（required ではないため ruleset への影響なし。`egress-isolation` は #1387 の指定で追加済み） |

## 8. オーナー判断

### 8.1 見直し（2026-11-02・#1003）

- **判断日**: 2026-11-02。根拠は macOS・Windows の実測（xos-measure の suite `egress-isolation`）。§8.2 の暫定判断（2026-10-31）を、この実測を受けて見直した
- 対応: TASK-38・`RENDER-3`・`SEC-2`・`XOS-1`

| 論点 | 2026-10-31 の暫定（§8.2） | 2026-11-02 の見直し | 後続 |
| ---- | ------------------------- | ------------------- | ---- |
| 1. GA の成立範囲 | Linux を先行（B: bwrap `--unshare-net`） | Linux（B: bwrap `--unshare-net`）と macOS（B: Seatbelt）を含める。根拠: macOS 26 の run 41401095620・41402439545 で、TCP 5 経路（redirect・img・fetch・ws・internal）と udp_direct が pass し、拒否ログの自己検査も成立した（予熱 5 回入り。#1415・#1424・#1425）。`dns`・`internal_real` は netns が前提の構造検査のため、macOS では対象外（unobservable） | 本番への組み込みは論点 4 |
| 2. 既知の制約の文言（D）の置き場所 | 設計 §2.5 か README のどちらか（未定） | README「実装方針（要点）」と設計 §2.5 に置く | 文言は両者で同じ内容 |
| 3. Windows の扱い | macOS・Windows の実測まで GA の成立範囲から外し、D の扱い | 引き続き D（既知の制約）とし、GA の成立範囲から外す。根拠: AppContainer 内で Servo ホストの陽性対照は成立した（`positive_ok: true`。run 41386861681・41402439545。#1413・#1420）。一方、Servo を使わない preflight で、capability なしでも loopback の外側 listener に届いた（loopback 免除を付けているため）。そのため全経路が「loopback listener では egress の遮断を測れない」理由で unobservable。測り直すには loopback 以外の宛先が要る | 測り方の検討は #1427、実装は #1515、実測は run 42000257965（#1519 で未観測の 3 経路を追跡）（宛先は TEST-NET-1。到達の有無は pktmon、試行の有無は WFP net events の drop で判定する。`harness/egress-isolation/README.md`）。AppContainer 内の ipc-channel の制約は #1421 |
| 4. 本番への組み込み | （新規の論点） | #1436（PR #1446）で実装済み。HostProcess の起動を Linux は bwrap、macOS は sandbox-exec で包み、通信を本体の許可リスト付きプロキシに限る（`[rendering] egress_isolation`。既定 `best-effort`、`required` は隔離できなければ起動しない）。依存・`unsafe` を足さず外部コマンドで包む方式にしたため、§7 の承認事項のうち依存・`unsafe` を要する案は採っていない。隔離中は WebSocket が使えず（Servo の `websocket_loader` はプロキシを使わない）、ファイルシステム全体は隔離しない（`--dev-bind / /`。#1485 で `/run`・`/tmp`・`$HOME` を、#1492 で `/var/tmp`・`/dev/shm`・WorkDir の親も tmpfs で隠し、WorkDir と隠れる場所にあるホストの実行ファイルだけ戻す）。実ホストでの検証は #1437 で `real_host` の `render3_egress_*`（`FANDHE_BROWSER_RENDER_EGRESS_ISOLATION=required`）に足し、xos-measure の suite `egress-host` で実測した（run 41592034373（main 169126a4）・41650422295（main 325ab8e8）で ubuntu・macOS とも `render3_egress_*` が 3 passed。記録は #1457。§9.5） | #1436・#1437 |

- 「8 経路すべて pass」（§6.4）のうち `internal_real`・`dns` は構造検査で、Servo 自身の DNS・UDP 送信の外部観測は #1461 の host-capture で追加し、run 41650422295（main）・41703923217（#1474 のブランチ。main の実測ではない）で観測した（§9.5）
- AppContainer 内の ipc-channel の制約（ボディ付きリクエストで panic し得る）は推定で、未実測（#1421）

### 8.2 暫定判断（2026-10-31・履歴。§8.1 で見直し済み）

- **判断日**: 2026-10-31。根拠は #1003 のコメント「オーナー判断（2026-10-31）: §8 の 3 点」で、Linux の実測（§6.4）を受けたもの
- **macOS・Windows の実測前の暫定判断**だった。実測の後に見直す、とされていた（§8.1 で見直した）
- 対応: TASK-38・`RENDER-1`・`SEC-2`・`XOS-1`

| 論点（判断前の問い） | 判断 | 後続 |
| -------------------- | ---- | ---- |
| 1. GA で必須とする成立範囲（3 OS 揃えるか、Linux を先行するか） | Linux を先行する。Linux は B（OS 隔離）として bwrap `--unshare-net` を主方式にする（§6.4 で 8 経路すべて pass）。docker `--network none` は、配布コンテナとホストの glibc をそろえてから測り直す | #1386（測定済み。§6.4・§9.1） |
| 2. B が成立しない OS・環境での既知の制約の文言（D） | OS 隔離が使えない環境では A（アプリ側の検知）だけで動作し、「egress を完全には遮断できない」と明記する。既定ではそのことを警告する | 文言の置き場所は §4 の候補（設計 §2.5・README）から選ぶ（未定。§8.1 の論点 2 で決定） |
| 3. Windows の扱い（弱い保証にとどめるか、対象外とするか） | macOS・Windows の実測が済むまで GA の成立範囲から外し、D の扱いとする | #1387（§8.1 の論点 1・3 で更新） |

- 「8 経路すべて pass」のうち `internal_real`・`dns` は構造検査で、Servo 自身の DNS・UDP 送信の外部観測は #1461 の host-capture で追加した。実測は §8.1・§9.1 を参照（§6.4）

## 9. 未確認事項

options §7 を引き継ぐ。本書で新たに加わった未確認事項と、2026-11-04 のオーナー判断（#1457）による残課題の扱いは §9.5 にまとめた。§6.2 で確かめるとしていた Docker 既定 seccomp 下での `unshare` の挙動と Landlock の実機の対応は、Linux（カーネル 7.0.0-34）について §6.4 で確かめた。

### 9.1 解消した事項（2026-10-31 の Linux 実測と以後の実測）

詳細は §6.4（TASK-38・`RENDER-1`）。

- macOS の成立性（B: Seatbelt の TCP 5 経路と udp_direct の pass）: 2026-11-02 の見直しで GA の成立範囲に含めた（§8.1。`dns`・`internal_real` は構造検査のため対象外）

- Docker 既定 seccomp 下の `unshare -n`: EPERM
- Ubuntu の AppArmor 制限（`kernel.apparmor_restrict_unprivileged_userns=1`）下での netns の作り方: 特権なしの `unshare -n`・`unshare -rn` は EPERM、`bwrap --unshare-net` は成功。ns 内に `CAP_NET_ADMIN` は無い
- Landlock の対応: カーネル 7.0.0-34 で LSM に有効・ABI 8（問い合わせのみ）
- 現在のコミットのホストでの再測定: c56f11c2 の dev ビルドで測り、bwrap `--unshare-net` は 8 経路すべて pass
- GitHub の ubuntu runner の AppArmor 下で bwrap が使えるか: Ubuntu 26.04 image の runner では使える（根拠は PR #1456 の同 image の CI ログだけで、他の image・AppArmor の設定は未確認）。同ログ（`rust-test-ubuntu`）に `egress isolation probe: bwrap available` が出た（#1458）
- docker `--network none` の判定（配布コンテナとホストの glibc をそろえた同一バイナリ。#1386）: 8 経路すべて pass（§6.4）
- Servo 自身の DNS・UDP 送信の外部観測（2026-11-04）: xos-measure `egress-host` の host-capture で観測した。run 41650422295 と #1474 のブランチの run 41703923217 で verdict pass。帰属できないもの（run 41650422295 では DNS 12・tcp_syn 54）は記録だけで合否に使わない（案 A）。関連は #1437・#1461・#1467・#1470・#1474・#1479・#1482・#1484（§9.5）
- 実ホストの子プロセス数（2026-11-04）: run 41650422295 の `EGRESS-PROCTREE` は 3 行で、`wrapper_children=1 init_children=1 init_children_main_thread=1`（#1460・#1466）
- 実ホストでの隔離下の動作（2026-11-04）: run 41592034373（main 169126a4）と 41650422295（main 325ab8e8）で、ubuntu・macOS とも `render3_egress_*` が 3 passed。記録は #1457（#1003 は CLOSED）

### 9.2 残る未確認事項と追跡先

| 未確認事項 | 追跡先 |
| ---------- | ------ |
| Windows の成立性（§3.3・§6.2）。§8.1 で D（既知の制約）とした。測り直しは次行の #1427 で追跡する | #1387 |
| Windows の測り直し（loopback 免除の下では外側 listener に届くため、loopback 宛てでは egress を測れない）。#1427 の予備実験で、TEST-NET-1 宛てとランナー側の観測（pktmon・WFP net events）で判定できる見込みを得て、#1515 でハーネスに実装した（`--appcontainer-testnet-observe`）。実測（xos-measure の suite `egress-isolation`）は run 42000257965（#1516 のブランチ head 9cb4f790 の実測。main の実測ではない。2026-11-05）で、`testnet_redirect`・`testnet_udp` が pass（容器の NIC 0 件・対照の NIC に出た・SID 一致の WFP drop あり）、`testnet_img`・`testnet_fetch`・`testnet_ws` は対照でも NIC に出ず unobservable（#1519 で追跡）。D の扱いは実測の結果を受けてオーナーが見直す | #1427・#1515・#1519 |
| AppContainer 内の ipc-channel の制約（ボディ付きリクエストで panic し得る。推定・未実測） | #1421 |
| Landlock ルール適用下の動作・seccomp との併用（保留。§9.5。許可リスト付きプロキシは #1434・#1440・#1436 で本番に実装済み。netns 内の中継 → UDS → プロキシの成立性は §6.5 で確認済み） | #1457 |
| bwrap のファイルシステム隔離の残り（`--ro-bind / /` 化・`/dev/log`・`/var/lib/*`・`/var/snap/*` 配下の UDS など、隠している場所以外の既知のソケット。`/run`・`/tmp`・`$HOME` の tmpfs 化は #1485、`/var/tmp`・`/dev/shm`・WorkDir の親〔`temp_root` が `/tmp` と `$HOME` の外にあるときの兄弟 WorkDir〕の tmpfs 化は #1492 で対応済み） | #1457 |
| 隔離中の WebSocket（現状は遮断のまま。WebSocket を必要とするサイトが実際に出たら見直す。§9.5） | #1457 |
| AppArmor の制限が無い環境の実測と転記（`egress-host-ubuntu-noaa`。仕組みは #1487・PR #1491 で入れた）・6.7 未満のカーネル（ホステッドランナーでは測れない。自前の環境が要る） | #1457・#1487 |
| net 層の直接接続の網羅 | options §7 の 1 |
| macOS の Seatbelt の UNIX ソケット宛の構文・他ユーザーの環境変数の可視性・トークン入り URI のログ経路（§10.6）。#1486 は調査済みで CLOSED。実装の要否はオーナー判断の後の別 issue | #1457 |

残課題 10 項目の扱い（2026-11-04 のオーナー判断）は §9.5 を正とする。

### 9.3 残余リスク（RENDER-3・SEC-2・TASK-38・#1458）

詳細は [servo-host-design.md](./servo-host-design.md) §2.5 の egress 隔離の残余リスク（(4)・(5)。#1451）と、`isolation.rs` のモジュール doc を正とする。ここでは要点だけを記す。

- Linux: FS 全体は隔離しない（`--dev-bind / /`）。`/run`・`/tmp`・`/var/tmp`・`/dev/shm`・`$HOME`・WorkDir の親は tmpfs で隠す（#1485・#1492）ため ssh-agent・X11・gnupg・他インスタンスのプロキシ UDS には届かないが、それ以外の場所（`/dev/log`・`/var/lib/*`・`/var/snap/*` 等の既知のソケット）の UDS を通る迂回は残る（拒否リスト型の限界）。tmpfs は容量だけ 1 本 64 MiB に抑える（#1498）。ただし bwrap が `--size` に対応していない版（upstream v0.7.0 より前）では、`egress_isolation = "required"` でも tmpfs は上限なし（既定の RAM の 50%）のまま、警告を 1 回出して起動する（2026-11-04 のオーナー判断・#1504 案 A）。`required` の意味は「egress を遮断できないなら起動しない」に限り、tmpfs の容量上限（メモリ DoS 対策）は含めない。egress の遮断は保たれ、迂回にはならない。`--size` の対応を検出できなかった場合も同じく上限なしで警告を出す（警告の文言で非対応と検出失敗を分ける。一過性の失敗は固定せず最大 3 回まで検出し直し、使い切ったらそのプロセスの間は上限なしに固定する。#1502・#1508）。また、inode の数（`nr_inodes`）は抑えられないため、空ファイルを大量に作るとカーネルのメモリを使いうる。`$HOME` が不正（未設定・システムのディレクトリ・`/` 等）だと隔離できない（`HomeNotHideable`）。WorkDir の親が `/` やシステムのディレクトリだと覆えず `WorkDirParentNotHideable`（回避策は `/tmp`・`/var/tmp`・`$HOME` 配下などを `temp_root` / `TMPDIR` に指定）。
- macOS: Seatbelt は IP の送信だけを絞る。mach/XPC 経由の通信は止めない。観測したのは `mDNSResponder` 経由の経路だけで、それ以外は観測していない。
- 測定（#1433）は `--ro-bind / /` で行ったが、本番は `--dev-bind / /` である。そのため、測定は本番の FS 上の UDS の迂回を覆っていない。#1485 の隠蔽の実地確認は、`isolation.rs` と `tests/host_process.rs` の本物の bwrap を使うテストが担う。実 Servo ホストでの確認はマージ後の xos-measure `egress-host`（人間が実行）。
- macOS のプローブは試走の終了コードだけで判定し、ns の比較のような偽物を排する手段が無い（Linux は #1459・#1466 で解決）。残余リスクとして受け入れる（2026-11-04・#1457）。
- 信頼判定の TOCTOU（#1466）: 実起動は、プローブ時にキャッシュした bwrap の実体パスを `select_trusted` で通し直さずに使う。実体は root 所有で祖先まで書き込み不可のまま、プロセスの生存中は変わらない前提に立つ。差し替えには root が要るため脅威モデルの外とする。

### 9.4 判断済みの事項（#1459）

- FS 上の UDS の迂回の塞ぎ方（2026-11-04・#1485。オーナー判断 #1457。#1492 で拡張）: bwrap で `--tmpfs /run`・`--tmpfs /tmp`・`--tmpfs /var/tmp`・`--tmpfs /dev/shm`・`--tmpfs <正規化した $HOME>`・`--tmpfs <WorkDir の親>` の後に、このインスタンスの WorkDir を `--bind`、隠れる場所にあるホストの実行ファイルを `--ro-bind` で戻す（順序が意味を持つ）。`--ro-bind / /`・Landlock・seccomp は使わない。`$HOME` が未設定・相対・システムのディレクトリ（`/` を含む）・存在しない・ディレクトリでないときは `HomeNotHideable` で隔離不能（`required` は拒否）。固定の対象（`/tmp` 等）は実体パスへ解決し、存在しないものは外し、祖先と子孫が重なるときは祖先だけを残す（`/tmp` が `/var/tmp` への symlink の環境でも 1 本になる）。WorkDir の親は起動ごとに決めて、既に隠れる場所の配下なら重複させず、そうでなければ tmpfs で覆う（兄弟 WorkDir の UDS を `temp_root` の置き場所によらず隠す。覆えない `/`・システムのディレクトリは `WorkDirParentNotHideable`）。親の tmpfs は起動ごとなのでプローブには含めず、覆えない場合は起動の失敗として表に出る（fail-open にはならない）。プローブは固定の対象と `$HOME` の tmpfs で走る。本体の `HOME` も子へ強制で渡す `HOME` も変えない（SEC-2）。ユーザーのフォントと `$HOME` の設定は見えなくなる。
- tmpfs の `--size`（#1498。#1492 の「付けない」を改める）: 各 `--tmpfs` の直前に `--size 67108864`（1 本 64 MiB）を付ける（`--size` は直後の 1 本にだけ効くため対象ごと）。導入は upstream bubblewrap v0.7.0（リリースノートの `--size` option）で、対応の有無は版の文字列ではなく `bwrap --help` に `--size` があるかの機能検出でプローブ時に決め、結果を `Sandbox::Bwrap` の `HiddenPaths` に持たせる（試走と本番の引数が構造上一致する）。非対応の版は `required` でも `SandboxProbeFailed` にせず、隔離を続けて上限なし（従来どおり既定の RAM の 50%）とする（`required` の意味を egress の遮断に限る扱い。オーナー判断 2026-11-04・#1504 案 A。残余リスクは §9.3）。検出結果は「`--help` を読めて `--size` が無い（非対応。確定なので固定）」と「検出の失敗」（起動の失敗・時間切れ・非 0 終了・出力 64KiB 超。上限なしで隔離を続け、`--size` は付けない）に分け、一過性の失敗（起動の失敗・時間切れ）は固定せず、次のプローブ時に最大 3 回まで検出し直す（回数は時間で区切らず、負荷が続く間に再起動が重なると短時間で使い切りうる。`Err` に終わった再プローブも 1 回に数え、使い切ったらそのプロセスの間は上限なしに固定する〔`TMPFS_REPROBE_MAX`。#1508〕。再プローブは試走を含む全体で、失敗しても前の `Ok` を残して隔離なしに降格させない。出力が上限を超えたら時間切れを待たず kill する。#1502）。`HostProcess` ごとに 1 回、`with_isolation_warning` のコールバック（`EgressIsolationWarning::TmpfsUnlimited`。未設定なら stderr）へ `warning: render host sandbox (bwrap) does not support --size; tmpfs mounts are not size-limited`（非対応）または `warning: render host sandbox (bwrap) --size support could not be detected; tmpfs mounts are not size-limited`（検出失敗）を出す（黙って緩めない）。上限の根拠: 子の `TMPDIR`・`HOME`・`XDG_*` は実ディスクの WorkDir を指すため正当な書き込みは tmpfs に入らず、入るのは `/dev/shm` の shm 程度。最悪の合計は最大 6 本 x 64 MiB = 384 MiB で RSS 上限（2 GiB）の約 19%、RAM の量によらない。64 MiB で Servo の実ホストが動くかは `real_host` と xos-measure の `egress-host` で確かめる（足りなければ起動の失敗として表に出て、隔離なしには落ちない）。残余リスク: `--size` は容量しか抑えず inode の数（`nr_inodes`。bwrap に指定手段が無い）は抑えない。tmpfs のページが RSS 監視に計上されない可能性は推測のまま。また親の tmpfs など起動後に bwrap が失敗しても `best-effort` で隔離なしに降格しない（`decide` を通るのは起動前の判定だけ。テスト `render3_parent_tmpfs_failure_never_downgrades_to_unisolated`・`render3_bwrap_runtime_failure_never_downgrades_to_unisolated`。#1498）。
- プローブの判定方法（2026-11-03）: Linux のプローブは終了コードに加えて、サンドボックス内の `readlink /proc/self/ns/net` の出力（netns の inode）が本体の値と異なることまで確かめる。exit 0 だけの偽物や、ns を作らない偽物は失敗する。詳細は `isolation.rs` の doc を正とする。
- 作業ディレクトリが `/run` 配下のときの扱い（2026-11-03）: `WorkDirHiddenBySandbox` で隔離不能とする fail-closed のまま変えない。回避策は `/run` の外を `temp_root` または `TMPDIR` に指定すること。2026-11-04 の判断でも変えない（#1457）。挙動は `isolation.rs` の doc を正とする。
- プローブの前提（2026-11-03・#1466）:
  - プローブは `env_clear` で走り、本番の起動は許可リストの環境変数で走る。差が出るのは起動できない側だけで、隔離なしに起動できてしまう fail-open にはならない。
  - プローブが確かめるのは、別の netns にいることまで。外部への出口が無いことは、その netns が本物の bwrap の `--unshare-net` で作られ `lo` しか持たないという前提に依存する。本物であることは root 所有の信頼判定で担保する。
  - netns に加えて pid ns（`/proc/self/ns/pid`）の分離も確かめる。rss 監視の pid 解決とラッパーの kill で ns 内のホストも落ちる挙動が、`--unshare-pid` に依存するため。busybox の `readlink` はオペランドを 1 つしか取らないので、名前空間ごとに別の bwrap 起動にする（結果はプロセスごとに 1 回キャッシュ）。

### 9.5 残課題の扱い（オーナー判断 2026-11-04・#1457・#1488）

TASK-38・`RENDER-3`・`SEC-2`・`XOS-1`。#1457 に集めた未起票の残課題について、2026-11-04 のオーナー判断と、その後の実測（xos-measure `egress-host`）・後続 PR の結果を合わせて記録する。

| 項目 | 扱い（2026-11-04） | 根拠・issue / run |
| ---- | ------------------ | ----------------- |
| bwrap の FS 隔離 | `/run`・`/tmp`・`$HOME` を tmpfs で隠し、WorkDir と実行ファイルだけを戻した。FS 全体の読み取り専用化（`--ro-bind / /`）はしない（ソケットへの connect は読み取り専用のマウントでも通るため）。残る迂回路は §9.3 | #1485（PR #1490）・§9.4 |
| 隔離中の WebSocket | 遮断のまま（fail-closed）。Servo の `websocket_loader` はプロキシを通らない。WebSocket を必要とするサイトが実際に出たら見直す | #1457 |
| Landlock・seccomp との併用 | 保留。依存か `unsafe` が要るうえ、#1485 の FS 隠蔽の後では得られるものが小さい。進めるときは dependency-policy の 5 項目を添えて、別途ユーザー承認を得る（依存の追加・`unsafe` の新規追加はいずれもユーザー承認制） | #1457・§7 |
| macOS の UDS 化 | 調査済み（§10）。Servo 0.7.0 の proxy は TCP しか扱えず、現実的な案は接続トークン。実装するか・どう実装するかはオーナー判断待ち | #1486（PR #1489）・#1457 |
| Docker 内での bwrap | 配布用コンテナには入れない。bwrap を動かすには特権か userns の許可が要り、コンテナ自体が弱くなるため。現行の `docker/Dockerfile.release` の既定・軽量コンテナは Servo ホストを含まず、Servo ホスト用の glibc 系の別イメージ（設計 §13 項目 8）も同じ方針。コンテナ内ではコンテナ側のネットワーク隔離（`docker run --network` 等）を使う。隔離できるかの判定はコンテナかどうかを見ない（コンテナを検出する仕組みは無い）。決め手は `/usr/bin/bwrap` の有無（無ければ `SandboxToolNotFound`）、信頼判定（root 所有でない等は `SandboxToolUntrusted`）、プローブの結果の 3 つだけ。現行の配布イメージには bwrap が無いため `SandboxToolNotFound` で隔離不能になり、`required` は起動を拒否し、`best-effort` は警告つきで隔離なしで起動する。Docker の既定 seccomp では `unshare -n` が EPERM だった（§6.4）ため、bwrap があってもプローブは失敗すると見込むが、Docker 内で bwrap のプローブを走らせたことはなく推定である。bwrap を同梱し userns や seccomp の設定を緩めた別のイメージでは、プローブが通って隔離が成立するなど挙動が変わりうる（未実測） | #1457・§6.4 |
| Servo 自身の DNS・UDP 送信の外部観測 | 解決済み。host-capture で観測した（run 41650422295〔main 325ab8e8〕と、#1474 のブランチ `fix/host-capture-end-canary` の run 41703923217〔main の実測ではない〕で verdict pass。帰属できないものは記録だけ〔案 A〕） | #1437・#1461・#1467・#1470・#1474・#1479・#1482・#1484。run 41650422295（main）・run 41703923217（#1474 のブランチ。main の実測ではない） |
| 作業ディレクトリが `/run` 配下 | 現状の fail-closed（`WorkDirHiddenBySandbox`）のまま | #1459・§9.4 |
| AppArmor の制限が無い環境・カーネル 6.7 未満 | 制限なしは `egress-host-ubuntu-noaa`（ubuntu-latest で `kernel.apparmor_restrict_unprivileged_userns=0`）で測る仕組みを入れた。実測と転記はまだ。ubuntu-22.04 は Servo 0.7.0 の freetype-sys が freetype 2.12 以上を要求しビルドできないため使わない（run 41813316652）。6.7 未満はホステッドランナーでは測れず（ubuntu-latest は 7.0.0 系。run 41813316652・41813309725）、自前の環境が要るので未検証 | #1487（PR #1491）・`harness/egress-isolation/README.md` |
| プローブで外へ出られないことまで確かめる | Linux は解決済み（netns・pid ns の inode を本体と比べ、ns を作らない偽物は通らない）。macOS は終了コードの判定のままで、残余リスクとして受け入れる（§9.3） | #1459・#1466 |
| 実ホストの子プロセス数 | 解決済み。`EGRESS-PROCTREE` の実測（run 41650422295）で `wrapper_children=1 init_children=1 init_children_main_thread=1` | #1460・#1466 |

- 実ホストでの隔離下の動作は run 41592034373（main 169126a4）・41650422295（main 325ab8e8）で確認済み（ubuntu・macOS とも `render3_egress_*` が 3 passed。記録先は #1457）。
- GitHub の ubuntu ランナーの AppArmor 下で bwrap が使えるかは解決済みで、§9.1 に記録している（PR #1456 の CI ログ・Ubuntu 26.04 image・#1458）。

## 10. macOS のプロキシの UNIX ソケット化の調査（#1486・TASK-38・RENDER-3・SEC-2・XOS-1）

調査と文書化だけを行う節で、コードは変えない。実装の要否と方式はオーナーが決める（#1457 の判断で調査が指示された）。確度は冒頭の表記に従う。一次情報は `servo-net 0.7.0`・`hyper-util 0.1.21`（`servo-host/Cargo.lock` が固定する版。crates.io から展開したソースを読んだ。上流の対応ファイルは Servo の `v0.7.0` タグの `components/net/connector.rs` と hyper-util の `v0.1.21` タグ）。

### 10.1 結論

- Servo 0.7.0 の proxy は TCP の宛先しか取れず、Servo を改変しない限り UNIX ソケットを指せない（確認済みの事実から導いた結論）。
- macOS にはネットワーク名前空間が無く、ホスト内の中継を置いても Servo から中継までは loopback の TCP になる。中継の listener が他ユーザー・他プロセスから届く点は今と変わらず、穴は中継に移るだけで閉じない（導いた結論。Seatbelt の個別構文は未確認だが採否に影響しない）。
- 現実的な対策は接続トークン（`Proxy-Authorization`）で、Servo を改変せずに付けられる。論点は秘密の渡し方で、argv は macOS で他ユーザーにも見えるため使えない。推奨候補は stdio の初回メッセージか、WorkDir 配下の 0600 ファイル。決定はオーナーが行う。

### 10.2 Servo 0.7.0 の proxy が UNIX ソケットを扱えない根拠

| 事項 | 根拠 | 確度 |
| ---- | ---- | ---- |
| proxy は pref の文字列（`network_http_proxy_uri`）から `Matcher::builder()` で組む | `servo-net-0.7.0/connector.rs` 604–605 | 確認済み |
| 文字列は `http::Uri` として解釈する。対応する scheme は http・https・socks 系で、authority が必須。`unix:` のような宛先を表す形は無い | `hyper-util-0.1.21/src/client/proxy/matcher.rs` の `build`（304 付近）・`parse_env_uri`（333 以降） | 確認済み |
| proxy へは `Tunnel::new(intercept.uri().clone(), self.client.clone())` で接続し、内側の `self.client` で `proxy_dst` に dial する。`ProxyConnector` の戻り値は TCP 固定 | `connector.rs` 631、`tunnel.rs` 133–134 | 確認済み |
| hyper-util の `HttpConnector` に UNIX ソケットへ dial する経路は無い。`unix` が出るのは `Connection` の trait 実装（601–602 行）だけ | `legacy/connect/http.rs` を grep（`unix` は 601–602 のみ） | 確認済み |
| userinfo は http 系で Basic 認証になり、`tunnel.with_auth` で CONNECT の `Proxy-Authorization` になる（`set_sensitive(true)`） | `matcher.rs` 355–375、`connector.rs` 632–636、`tunnel.rs` 78–87 | 確認済み |
| `TunnelError` の `Display` は固定文言で、proxy の URI（トークンを含みうる）を出さない。Servo はこれを `ProxyError(format!("{e}"))` に包む | `tunnel.rs` 235–247、`connector.rs` 625・638 | 確認済み |

### 10.3 代替案の比較

| 案 | 実現性 | 残る穴 | 依存・`unsafe` | 測定 |
| -- | ------ | ------ | -------------- | ---- |
| UNIX ソケットを Servo の proxy に直接指定 | 不可（10.2）。Servo を改変すると MPL のソース開示義務が生じ、licensing の方針にも反するため採らない | — | — | — |
| (a) Linux と同じホスト内中継（`--egress-relay-uds`）を macOS でも使う | 成立しない。Servo から中継へは loopback の TCP で、Seatbelt で `remote ip` を全面拒否すると Servo 自身が中継に届かない。許可すれば中継の listener が今の穴を引き継ぐ。`isolation.rs`・`relay.rs` の doc も、隔離なしだとホストの 127.0.0.1 に中継が開くと指摘している（確認済み） | 他ユーザー・他プロセスから中継経由で本体の送信元 IP が使える（今と同じ） | std で足りる見込み（推測）。`unsafe` 不要 | Seatbelt の UNIX ソケット宛の許可は `(remote unix-socket (path-literal ...))` の形が Apple 同梱・公開 profile にあるとされるが、実機では未確認。採否には影響しない |
| (b) 接続トークン（`Proxy-Authorization`）を Servo に付けさせる | 実現可能。`LoopbackProxy::uri()`（`servo-host/src/engine.rs`）を `http://<user>:<token>@127.0.0.1:<port>` にすれば Servo が付ける（10.2）。本体プロキシ側で検証し、不一致を 407 で閉じる実装を足す（将来の実装であり、現状の `egress_proxy/request.rs` は `Proxy-Authorization` を検証せず 407 も返さない。確認済み。未検証） | 同じ uid のプロセスは WorkDir・環境変数・プロセス情報を読めて防げない。検証が欠けると今の状態に戻る | トークン生成に CSPRNG が要る。std だけで足りるかは未確認で、依存が要るなら承認事項。`unsafe` は不要の見込み（推測） | xos-measure の macOS（`egress-isolation`・`egress-host`）で、陽性対照（トークン付きで通る）と陰性対照（なし・別トークンで 407）を測れる見込み（推測） |
| (c) 本体側で接続元プロセスを確かめる | TCP の接続元 pid を得るには `proc_pidfdinfo` 等の FFI が要る（推測）。`LOCAL_PEERPID` は UNIX ソケット専用 | pid の再利用の競合 | `unsafe` が要るため承認事項。推奨しない | 未確認 |

Seatbelt の profile は隔離したプロセスの送信だけを絞り、他のプロセスから listener への受信は絞れない。そのため、Seatbelt は listener を他から守る手段にならない（導いた結論）。

### 10.4 トークン案の SEC-2 上の扱い

`Proxy-Authorization` は CONNECT のときだけ送られ、本体プロキシが読んで終わる。本体プロキシは CONNECT のヘッダ行を検証するだけ（`egress_proxy/request.rs` の `parse_connect`・`validate_header_line`）で、トンネル確立後のバイト列は改変しない。そのため origin から見える識別面（UA・ヘッダ・`navigator`・TLS）は変わらない見込み（確認済みの動作からの推論）。宛先の許可判定と接続時の IP 再検査は本体プロキシのままで、トークンは送信元 IP の踏み台化を減らす対策であり、SSRF 対策の代わりではない。

### 10.5 トークンの渡し方の候補（決定はオーナー）

| 渡し方 | 評価 |
| ------ | ---- |
| argv（現状の `--egress-proxy-port` と同じ） | 不可。macOS の `ps` で他ユーザーにも見える |
| 環境変数 | 他ユーザーの環境変数の可視性は macOS で未確認。`HostProcess` は許可リストの環境変数で起動するため拡張も要る |
| stdio プロトコルの初回メッセージ | 有力。ホスト起動後の握手で渡す。プロトコルとスキーマの変更が要る。設計 §2.6 の「クライアントから任意引数を渡さない」とは、本体が決めた値を渡す形なので整合する |
| WorkDir 配下の 0600 ファイル | 有力。同じ uid 以外は読めない（Linux の UNIX ソケット配置と同じ考え方）。パス検証が要る |

実装時の条件（後続 issue）: 起動ごとに新規に作る。比較は定数時間で行う。407 応答とログにトークンと `Proxy-Authorization` の値を出さない。トークン入りの proxy URI が Servo の他のログ経路（pref のダンプ・デバッグ出力・`ProxyError` 以外のエラー）に出ないことを確かめる（受け入れ条件。§10.6）。トークン検証が使えないときは起動を拒否する（`required` の既存方針と同じく fail-closed）。

### 10.6 未確認事項と後続

- macOS 実機での Seatbelt の構文（UNIX ソケット宛の許可・拒否）と、`seatbelt.py` の拒否行の帰属。現在の `_DENY_RE` は `ip:port` と `*:port` の形だけを受け付けるため、UNIX ソケット宛の拒否行は帰属できず、測るには拡張が要る。
- 他ユーザーのプロセスの環境変数の macOS での可視性。
- トークン生成に std だけで足りるか。
- トークン入りの proxy URI（`http://user:token@127.0.0.1:port`）は Servo の pref として渡る。§10.2 で確かめたのは `TunnelError` の Display だけで、pref のダンプ・デバッグ出力・`ProxyError` 以外のエラーに URI が出ないことは未確認（#1489 の監査 P2）。
- 実装（`servo-host` の `engine.rs`・`args.rs`、`egress_proxy` の 407、`host_process` の受け渡し、`real_host` と xos-measure のテスト追加）は、オーナー判断の後の別 issue。設計 §2.5 の (3) と `isolation.rs` の「別途検討」の更新もそのときに行う。
- 依存の追加・`unsafe`・`ci.yml` の変更は本 issue では行っていない。
