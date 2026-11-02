# Okiteru?

Tailscale につなぐだけで、LAN 内の PC をスマホから起こして(Wake-on-LAN)、お休みさせて(休止・スリープ・シャットダウン)、様子を見られる中継ツールです。

*English: a single-binary Wake-on-LAN relay for your tailnet. Install it on one always-on PC (Windows; Linux and macOS are supported too, less tested) with Tailscale, and wake, monitor and put to sleep your other LAN machines from your phone. No configuration; the LAN broadcast address, NIC and MAC addresses are found automatically. The web UI is available in Japanese and English (it follows the browser language; switch it from the selector in the header); logs, the command-line help and the docs are in Japanese.*

- **設定不要:** 送信元の NIC とブロードキャストアドレスは自動で求めます。起動中の機器は、tailnet の端末一覧と ARP から見つけて、選ぶだけで登録できます(MAC の手入力は不要)。
- **複数台:** 機器の追加・削除・名前の変更は画面から。機器ごとに、起動の確認に使う TCP ポートを選べます。
- **監視:** 画面に「起きています/お休み中です」を出します。起こしたあとは、起きるまで待ちます。画面を開いていなくても、起動と停止をログに残します。
- **起動予約:** 機器ごとに、決めた時刻に自動で起こします(毎週の曜日指定、または一度だけ)。
- **再送:** 起動要求のあと、起きたことを確認できなければ、30 秒おきに最大 3 回まで自動で送り直します。それでも起きなければ失敗として画面に出します。
- **通知:** 起きたとき、起きなかったとき、お休みしたときに、スマホへ通知します(Web Push。HTTPS が必要)。
- **日本語・英語:** 画面は日本語と英語で表示できます。ブラウザの言語に合わせて選び、画面の右上でも切り替えられます。
- **PIN での認証(任意):** tailnet の中でも、操作できる人を PIN で絞れます。既定では無効です。
- **お休みなさい:** 相手の PC を休止状態・スリープ・シャットダウンにします(機器ごとに選択。相手側に agent が必要)。
- **導入が簡単:** Rust 製の単一バイナリ(Python などは不要)。Windows ではサービスとして自動起動します。スマホにはアプリとしてインストールできます。

## 作った理由

[MulmoTerminal](https://www.mulmoterminal.com/guide/ja/) は、Claude Code などの AI コーディングエージェントを複数動かして、ブラウザの 1 画面で見張るための「管制室」です。私は、自宅の PC で MulmoTerminal を動かし、外出先からスマホの Tailscale 経由で操作しています。

リモート デスクトップ(RDP)なら何でもできますが、モバイル回線では少し重く感じます。そこで、HTTP だけで操作できる仕組みを求めていました。

ただ、MulmoTerminal が HTTP で操作できても、PC が寝ていたら届きません。そこで、起こす(Wake-on-LAN)のも、休止させるのも、スマホの同じ画面でできる中継として、okiteru を作りました。これで、PC を起こす、MulmoTerminal を操作する、PC を休止させる、までを、すべて HTTP で行えます。

## 仕組み

```
スマホ ──(Tailscale)──▶ 常時起動の PC [okiteru] ──マジックパケット(LAN)──▶ 起こしたい PC
                                │ └──(Tailscale)「お休みなさい」──▶ 止めたい PC [okiteru --agent]
                                └── 起動の確認: 機器ごとの TCP ポートへ接続
```

## 必要なもの

- 常時起動の PC 1 台(中継)。Windows、Linux(x86_64・aarch64。ラズパイなど)、macOS に対応します。Tailscale を入れ、LAN につないでおく
- スマホ(Tailscale を入れ、同じ tailnet に参加)
- 起こす PC は **有線 LAN** でつなぎ、Wake-on-LAN を使える設定にする([下記](#起動される側の-pc-の設定))

## 導入(Windows)

1. [`okiteru.exe` をダウンロード](https://github.com/highfar-projects/okiteru/releases/latest/download/okiteru.exe)して、中継にする PC に置く(一覧は [Releases](https://github.com/highfar-projects/okiteru/releases)。同じ場所の `okiteru.exe.sha256` でチェックサムを確かめられます)。
2. **管理者として実行した PowerShell** で次を実行する。

   ```powershell
   .\okiteru.exe install
   ```

   実行ファイルを `C:\Program Files\okiteru\` にコピーし、サービス `okiteru` を自動起動で登録して開始します。
   Tailscale の範囲(100.64.0.0/10)からの接続だけを許すファイアウォール規則も追加します。
   終わりに表示される `http://<この PC の Tailscale IP>:8765/` が、スマホで開く URL です。
3. スマホで Tailscale を有効にして、その URL を開く。
4. 起こしたい PC を起動した状態で「＋ 追加」→「探す」から登録する。

新しい版に入れ替えるときも、新しい `okiteru.exe` で同じ `install` を実行します(設定は残ります)。

サービスは最小権限の `LocalService` で動きます。Tailscale の情報を読めないなど、権限が足りないときは `install --system` で `LocalSystem` に切り替えられます。

## 導入(Linux)

Linux は、中継も agent も使えます。**開発の環境(WSL2 上の Ubuntu)での前面実行は確かめていますが、systemd への登録は実機で通して確かめていません**(ユニットファイルは `systemd-analyze verify` を通ります)。

1. [Releases](https://github.com/highfar-projects/okiteru/releases) から、環境に合う `okiteru-linux-x86_64` または `okiteru-linux-aarch64` をダウンロードして、実行権限をつける(`chmod +x`)。
2. root 権限で登録する。

   ```sh
   sudo ./okiteru-linux-x86_64 install            # 中継
   sudo ./okiteru-linux-x86_64 install --agent    # 起動される側(お休みなさいの受け口)
   ```

   実行ファイルを `/usr/local/bin/okiteru` にコピーし、systemd のサービス `okiteru`(agent は `okiteru-agent`)を登録して開始します。再起動後も自動で始まります。
   設定とログは `/var/lib/okiteru/` に置きます。削除は `sudo okiteru uninstall`(agent は `--agent` を付ける)。
3. 終わりに表示される `http://<この PC の Tailscale IP>:8765/` を、スマホで開く。

- サービスは root で動きます。`tailscale` の操作用ソケットが root にしか開かれていないことが多く、agent は電源操作(`systemctl hibernate` / `suspend` / `poweroff`)に権限が要るためです。
- 休止状態は、Linux 側で休止が使えるときだけ動きます(スワップの設定などが要ります)。使えないときは、スリープかシャットダウンを選んでください。
- 起こされる側として使うときは、Wake-on-LAN を有効にします(`ethtool -s <NIC> wol g`。再起動すると戻るので、NetworkManager や systemd-networkd で永続化します)。

## 導入(macOS)

macOS も、中継と agent の両方に対応します。**macOS の実機では動作を確かめていません。** `cargo test` を macOS の CI で通すところまでです。

1. [Releases](https://github.com/highfar-projects/okiteru/releases) から `okiteru-macos-aarch64`(Apple Silicon)をダウンロードして、実行権限をつける。初回は、Gatekeeper に止められたら「システム設定 → プライバシーとセキュリティ」から許可します(署名・公証はしていません)。
2. root 権限で登録する。

   ```sh
   sudo ./okiteru-macos-aarch64 install            # 中継
   sudo ./okiteru-macos-aarch64 install --agent    # 起動される側
   ```

   launchd のデーモン(`net.highfar.okiteru`・`net.highfar.okiteru-agent`)として登録します。設定とログは `/Library/Application Support/okiteru/` に置きます。
3. Tailscale は、アプリ版(`/Applications/Tailscale.app`)をそのまま使えます。
4. macOS には、スリープとは別の「休止状態」の操作がありません。agent の機器は、スリープかシャットダウンを選んでください(休止状態を選ぶと、相手が断ります)。
5. 起こされる側の macOS は、「システム設定 → バッテリー(または省エネルギー)」の「ネットワークアクセスによるスリープ解除」を有効にします。

## スマホのホーム画面に追加(アプリとしてインストール)

- **iPhone:** Safari の共有メニューから「ホーム画面に追加」。HTTP のままで追加できます。
- **Android(Chrome):** 「インストール」は HTTPS のときだけ出ます。`tailscale serve` で HTTPS にします(管理コンソールの DNS ページで、MagicDNS と HTTPS Certificates を有効にしておく)。

  ```powershell
  tailscale serve --bg --https=443 http://127.0.0.1:8765
  ```

  スマホで `https://<この PC の名前>.<tailnet 名>.ts.net/` を開き、メニューから「アプリをインストール」を選びます。止めるときは `tailscale serve --https=443 off`。`tailscale funnel` は公開インターネットに出るので使わないでください。

## 「お休みなさい」(休止・スリープ・シャットダウン)

スマホの「お休みなさい」ボタンで、相手の PC を休止状態(既定)・スリープ・シャットダウンにできます。
中継から相手の PC を止めるには、相手側に受け口(agent)が要ります。**止めたい PC に** Tailscale を入れた上で、管理者の PowerShell で次を実行します。

```powershell
.\okiteru.exe install --agent
```

- サービス `okiteru-agent`(LocalSystem)として登録され、ポート 8766 で、Tailscale と localhost からの要求だけを受けます。
- 認証は Tailscale に任せています。同じ tailnet の端末なら誰でも頼めるので、制限したいときは Tailscale の ACL で 8766 を絞ってください。
- 「探す」から追加した機器は、相手の Tailscale の IP が自動で入ります。手で追加した機器は、機器の「設定」で入力してください。IP が入った機器にだけボタンが出ます。
- する操作は、機器の「設定」で機器ごとに選びます。
- 休止状態は、Windows で休止が有効なときだけ使えます(`powercfg /hibernate on`)。スリープは、休止が有効だと休止になることがあります(Windows の仕様)。シャットダウンは強制ではないので、保存を求めるアプリが開いていると止まらないことがあります。
- 削除は `.\okiteru.exe uninstall --agent`。ログは `C:\ProgramData\okiteru\okiteru-agent.log`。

## 起動予約

機器の「予約」から、時刻と曜日(または日付)を決めます。時刻になると、「起きてください」と同じ処理でマジックパケットを送ります。

- 時刻は **中継の PC のローカル時刻**です。ログの時刻は UTC なので、ずれて見えます。
- 時刻から 5 分以内なら実行します(サービスの再起動の直後でも拾います)。それより遅れた分は実行しません。
- 同じ時刻を二度は実行しません(再起動をまたいでも)。一度きりの予約は、実行すると止まります。
- 中継が止まっていた間の予約は実行しません。
- 予約は `devices.json` と同じ場所の `schedules.json` に保存します。機器を削除すると、その機器の予約も消えます。
- 実行と結果は `okiteru.log` に残ります(要求元は「予約」)。

## 通知

画面右上の「通知: オフ」を押して許可すると、次のときにスマホへ通知が届きます。画面を閉じていても届きます。

- 起動要求のあと、起きたことを確認できたとき(起動予約・再送のあとも含む)
- 再送しても起きなかったとき
- 「お休みなさい」のあと、止まったことを確認できたとき

使うには次が要ります。

- **HTTPS で開くこと。** ブラウザの決まりで、HTTP のままでは通知を使えません。[HTTPS にする方法](#スマホのホーム画面に追加アプリとしてインストール)と同じく `tailscale serve` を使います。
- **iPhone は、ホーム画面に追加したアプリから開くこと。** Safari のタブでは通知を使えません。
- 通知は、**中継の PC から、各ブラウザの Push サービス**(Chrome・Edge は Google、Firefox は Mozilla、Safari は Apple、Windows のブラウザは Microsoft)へ、HTTPS で送ります。インターネットへの接続が要ります。本文は端末の鍵で暗号化するので、Push サービスには読めません。
- 送り先は、上のサービスのホストに限っています(それ以外の通知先は、登録できません)。
- 送り主の鍵と、通知を受け取る端末の情報は、`push.json` に保存します。この鍵は、Push サービスに対して中継を名乗るためのものです。ほかの人に渡さないでください。
- 端末ごとの入り切りは、その端末で「通知: オン/オフ」を押します。機器ごとの入り切りはありません。

## 認証(PIN)

既定では、認証は Tailscale に任せていて、同じ tailnet の端末なら誰でも操作できます。tailnet を家族や同僚と共有しているなど、操作できる人を絞りたいときは、PIN を設定できます(任意)。

```sh
okiteru auth set      # PIN を設定して、認証を有効にする(標準入力から読む。6 文字以上)
okiteru auth status   # 有効かどうか
okiteru auth revoke   # ログイン中の端末をすべて外す(PIN はそのまま)
okiteru auth off      # 認証を無効にする
```

- **中継の PC で実行します。** 画面からは PIN を設定できません(tailnet の誰かに、勝手に設定されないようにするためです)。サービスの再起動は要りません。すぐ効きます。Windows は、管理者の PowerShell で実行してください。
- 有効にすると、画面を開いたときにログインの画面が出ます。ログインすると、その端末は 30 日間そのままです(画面右上の「ログアウト」で外せます)。
- 守るのは `/api/` の操作と閲覧のすべてです(機器の一覧も含みます)。画面のファイル(HTML・JS)だけは、ログイン前にも出ます。機器の情報は入っていません。
- PIN は、ハッシュ(PBKDF2-HMAC-SHA256、塩つき)にして `auth.json` に保存します。PIN そのものは、ファイルにもログにも残りません。ログイン中の端末には、署名つきの Cookie(`HttpOnly`・`SameSite=Strict`)を渡します。
- **失敗が続くと止めます。** 5 回続けて間違えると 1 分、さらに間違えると 2 分、4 分…と増えて、最長 15 分、ログインを受け付けなくなります。`tailscale serve` の後ろでは、接続元がすべて localhost に見えるため、端末ごとではなく**全体で数えます**。止めている間も、ログイン済みの端末は使えます。
- PIN を変える(もう一度 `auth set`)か `auth revoke` をすると、ログイン中の端末がすべて外れます。
- `auth.json` が壊れていて読めないときは、安全側に倒して、すべての操作を断ります(`auth off` か、ファイルの修復が必要です)。

**この認証が守らないもの**
- **agent(起動される側の PC のポート 8766)は、守りません。** 同じ tailnet の端末なら、PIN なしで「お休みなさい」を頼めます。絞りたいときは、Tailscale の ACL で 8766 を絞ってください。
- 通信そのものは暗号化しません。Cookie は、HTTP のままなら Tailscale の WireGuard の上を流れます。`tailscale serve` で HTTPS にしたときも、Cookie に `Secure` は付けていません。
- PIN は、短い文字列です。長い失敗の止め方で守っていますが、強い守りが要るなら、Tailscale の ACL と併用してください。
- `auth.json` は、Linux・macOS では自分だけが読める権限で作ります。Windows では、`C:\ProgramData\okiteru\` の権限に従います(ほかのローカルユーザーが読める場合があります)。

## 言語

画面は日本語(`ja`)と英語(`en`)に対応しています。最初の言語はブラウザの設定(`navigator.languages`)から選び、どちらでもなければ英語です。画面の右上の選択で切り替えると、その端末のブラウザに保存されます。

サーバーが返すエラーは、文と一緒に種類(`code`)と補足(`params`)を返し、画面が選んだ言語の文を作ります。知らない `code` のときは、サーバーの文(日本語)をそのまま出します。

次のものは、今のところ日本語のままです。

- ログ(`okiteru.log`)と、コマンドラインの使い方の表示。調べものに使うので、日本語に固定しています
- スマホに届く通知の文面(サーバーが作って送るため)
- `manifest.webmanifest` の説明文

### 言語を足す

1. `web/lang/ja.json` をコピーして、`web/lang/<言語コード>.json`(例: `fr.json`)を作り、値だけを訳します。`{name}` のような `{...}` は、そのまま残します(実行時に値が入ります)。
2. `web/i18n.js` の `LANGUAGES` に `fr: "Français"` を足します。
3. `src/api.rs` の `asset` に `/lang/fr.json` を足します(ファイルはバイナリに埋め込まれます)。
4. `cargo test` を実行します。キーや `{...}` が `ja.json` とそろっていないと、テストが落ちます。

画面に出す文を足すときは、`web/index.html` では `data-i18n="キー"`、`web/app.js` では `t("キー")` を使い、キーを `ja.json` と各言語に足します。サーバーに新しいエラーを足すときは、`coded(...)` か `Detail::new(...)` で `code` を付け、`err.<code>` を辞書に足してください(抜けていればテストが落ちます)。

## 起動される側の PC の設定

- BIOS/UEFI で Wake on LAN(「PCIe による電源オン」など)を有効にする
- NIC の「マジック パケットでのみスタンバイ状態を解除する」を有効にする(デバイス マネージャー → ネットワーク アダプター → 電源の管理)
- 高速スタートアップを無効にする(シャットダウンからの起動が効かなくなることがあります)
- 有線 LAN でつなぐ。登録する MAC は有線 NIC のもの

## 設定とログ

| 内容 | 場所(Windows) |
|---|---|
| 機器の一覧 | `C:\ProgramData\okiteru\devices.json` |
| 起動予約 | `C:\ProgramData\okiteru\schedules.json` |
| 通知の鍵と購読 | `C:\ProgramData\okiteru\push.json` |
| 認証(PIN のハッシュ) | `C:\ProgramData\okiteru\auth.json` |
| ログ(中継) | `C:\ProgramData\okiteru\okiteru.log`(256KB で `.log.old` に切り替え。1 世代だけ残る) |
| ログ(agent) | `C:\ProgramData\okiteru\okiteru-agent.log` |

Linux・macOS では、サービスとして登録した場合は `/var/lib/okiteru/`(Linux)・`/Library/Application Support/okiteru/`(macOS)に、前面で実行した場合は `~/.config/okiteru/` に、同じ名前のファイルを置きます。

ログには、起動要求・お休み要求、起動と停止の確認、機器の追加・変更・削除を、要求元の IP とともに記録します。**時刻は UTC** です(日本時間は +9 時間)。起動と停止の確認は 10 秒ごとなので、時間の精度も 10 秒程度です。

## うまくいかないとき

- **「探す」で何も出ない:** 起こしたい PC が起動していて、Tailscale が動いている必要があります。サービスが `tailscale` を実行できないときは `install --system` を試してください。手で入力して追加することもできます。
- **起こしても起きない:** [起動される側の PC の設定](#起動される側の-pc-の設定)を確認してください。登録した MAC が有線 NIC のものか、有線でつながっているかも確認してください。
- **起きたのに「お休み中です」のまま:** 機器の「確認ポート」が開いていません。リモート デスクトップなら 3389、ファイル共有なら 445 です。Windows は ping に応答しないので、TCP ポートで確認しています。
- **画面が開けない:** スマホで Tailscale が有効か、URL の IP が中継 PC の Tailscale IP かを確認してください。

## 安全面で守っていること

- localhost と、この PC の Tailscale の IP でだけ待ち受けます(`0.0.0.0` では待ち受けません)。接続元も、localhost と Tailscale の範囲(100.64.0.0/10)に限ります。
- `Host` を検査し(DNS リバインディング対策)、`Origin` が `Host` と一致することを求め、変更の操作には `Content-Type: application/json` を必須にしています(他サイトからの操作を防ぐ)。
- 画面は、インラインのスクリプトとスタイルを禁止する CSP の下で動きます。
- 起動の確認と agent への接続先は、LAN の私的アドレス、または Tailscale の範囲に限っています(任意のホストへ接続させません)。例外は通知で、Chrome・Firefox・Safari・Windows の Push サービスのホストに限った許可リストにだけ接続します。
- 自分のコードでは `unsafe` を使いません(例外は Windows サービスの入口 1 ファイルだけで、テストで他に増えていないことを確かめています)。

認証は、既定では Tailscale に任せています。同じ tailnet の端末なら、誰でも操作できます。絞りたいときは、[PIN の認証](#認証pin)を有効にできます。脆弱性を見つけたときは [SECURITY.md](SECURITY.md) を見てください。

## 制限

- Linux と macOS は、Windows ほど確かめていません(上の「導入」を参照)。macOS の Intel 版の実行ファイルは配っていません(ソースからビルドできます)。
- Windows 以外では、サービスは root で動きます。
- IPv4 の LAN だけを対象にしています。
- 起こす・止める相手は、中継と同じ LAN にいる機器です。

## ビルドとテスト

Rust 1.88 以降が要ります。

```sh
cargo test
cargo build --release   # target/release/okiteru(Windows は okiteru.exe)
```

CI は Windows・Linux・macOS で `cargo test` を通します。

アイコン(PNG)は `python3 tools/make_icons.py` で `web/icon.svg` と同じ絵から作り直せます。

## ライセンス

[MIT](LICENSE)
