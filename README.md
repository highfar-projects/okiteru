# okiteru

Tailscale につなぐだけで、LAN 内の PC をスマホから Wake-on-LAN で起動・監視する中継ツール(開発中)。

- 設定不要: 送信元 NIC とブロードキャストアドレスは自動で求める
- 起動中の機器を tailnet の端末一覧と ARP から見つけ、選ぶだけで登録
- 複数台対応、機器ごとの TCP ポートでの起動確認
- Rust 製の単一バイナリ。Windows サービスとして常駐する

状態: 開発中。実機での確認はこれから。`docs/prototype/wol_server.py` は動作確認済みの個人用試作(参考)。

## 導入(Windows)

LAN 内で常時起動している PC に、Tailscale を入れた上で導入します。

1. `okiteru.exe` を置き、**管理者として実行した PowerShell** で次を実行する。

   ```powershell
   .\okiteru.exe install
   ```

   実行ファイルを `C:\Program Files\okiteru\` にコピーし、サービス `okiteru` を自動起動で登録して開始する。
   Tailscale の範囲(100.64.0.0/10)からの接続だけを許すファイアウォール規則も追加する。
2. スマホで Tailscale を有効にして、表示された `http://<この PC の Tailscale IP>:8765/` を開く。
3. 起動したい PC を起動した状態で「＋ 追加」→「探す」から登録する。

削除は `.\okiteru.exe uninstall`。設定ファイル(`C:\ProgramData\okiteru\devices.json`)とログ(同じフォルダの `okiteru.log`)は残る。

サービスは最小権限の `LocalService` で動く。Tailscale の情報を読めないなど、権限が足りないときは `install --system` で `LocalSystem` に切り替えられる。

## 「お休みなさい」(休止・スリープ・シャットダウン)

スマホの「お休みなさい」ボタンで、相手の PC を休止状態(既定)・スリープ・シャットダウンにできます。
中継から相手の PC を止めるには、相手側に受け口(agent)が要ります。**止めたい PC に** Tailscale を入れた上で、管理者の PowerShell で次を実行します。

```powershell
.\okiteru.exe install --agent
```

- サービス `okiteru-agent`(LocalSystem)として登録され、ポート 8766 で、Tailscale と localhost からの要求だけを受けます。
- 認証は Tailscale に任せています。同じ tailnet の端末なら誰でも頼めるので、制限したいときは Tailscale の ACL で 8766 を絞ってください。
- 「探す」から追加した機器は、相手の Tailscale の IP が自動で入ります。手で追加した機器は、設定で入力してください。IP が入った機器にだけボタンが出ます。
- する操作は、機器の設定で機器ごとに選びます。
- 休止状態は、Windows で休止が有効なときだけ使えます(`powercfg /hibernate on`)。スリープは、休止が有効だと休止になることがあります(Windows の仕様)。
- 削除は `.\okiteru.exe uninstall --agent`。ログは `C:\ProgramData\okiteru\okiteru-agent.log`。

## 起動される側の PC の設定

- BIOS/UEFI で Wake on LAN を有効にする
- NIC の「マジック パケットでのみスタンバイ状態を解除する」を有効にする
- 高速スタートアップを無効にする
- 有線 LAN でつなぐ(登録する MAC は有線 NIC のもの)

## ライセンス

MIT
