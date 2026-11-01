# Okiteru?

Tailscale につなぐだけで、LAN 内の PC をスマホから起こして(Wake-on-LAN)、お休みさせて(休止・スリープ・シャットダウン)、様子を見られる中継ツールです。

*English: a single-binary Wake-on-LAN relay for your tailnet. Install it on one always-on Windows PC with Tailscale, and wake, monitor and put to sleep your other LAN machines from your phone. No configuration; the LAN broadcast address, NIC and MAC addresses are found automatically. UI and docs are in Japanese.*

- **設定不要:** 送信元の NIC とブロードキャストアドレスは自動で求めます。起動中の機器は、tailnet の端末一覧と ARP から見つけて、選ぶだけで登録できます(MAC の手入力は不要)。
- **複数台:** 機器の追加・削除・名前の変更は画面から。機器ごとに、起動の確認に使う TCP ポートを選べます。
- **監視:** 画面に「起きています/お休み中です」を出します。起こしたあとは、起きるまで待ちます。画面を開いていなくても、起動と停止をログに残します。
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

- 常時起動の Windows PC 1 台(中継)。Tailscale を入れ、LAN につないでおく
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

## 起動される側の PC の設定

- BIOS/UEFI で Wake on LAN(「PCIe による電源オン」など)を有効にする
- NIC の「マジック パケットでのみスタンバイ状態を解除する」を有効にする(デバイス マネージャー → ネットワーク アダプター → 電源の管理)
- 高速スタートアップを無効にする(シャットダウンからの起動が効かなくなることがあります)
- 有線 LAN でつなぐ。登録する MAC は有線 NIC のもの

## 設定とログ

| 内容 | 場所(Windows) |
|---|---|
| 機器の一覧 | `C:\ProgramData\okiteru\devices.json` |
| ログ(中継) | `C:\ProgramData\okiteru\okiteru.log`(256KB で `.log.old` に切り替え。1 世代だけ残る) |
| ログ(agent) | `C:\ProgramData\okiteru\okiteru-agent.log` |

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
- 起動の確認と agent への接続先は、LAN の私的アドレス、または Tailscale の範囲に限っています(任意のホストへ接続させません)。
- 自分のコードでは `unsafe` を使いません(例外は Windows サービスの入口 1 ファイルだけで、テストで他に増えていないことを確かめています)。

認証は Tailscale に任せています。同じ tailnet の端末なら、誰でも操作できます。脆弱性を見つけたときは [SECURITY.md](SECURITY.md) を見てください。

## 制限

- 中継の Windows サービス登録は Windows 専用です。Linux では前面での実行(`okiteru`)だけできますが、動作は未検証で、サービス登録(systemd)は未対応です。
- IPv4 の LAN だけを対象にしています。
- 起こす・止める相手は、中継と同じ LAN にいる機器です。

## ビルドとテスト

Rust 1.88 以降が要ります。

```powershell
cargo test
cargo build --release   # target\release\okiteru.exe
```

アイコン(PNG)は `python3 tools/make_icons.py` で `web/icon.svg` と同じ絵から作り直せます。

## ライセンス

[MIT](LICENSE)
