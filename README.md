# okiteru

Tailscale につなぐだけで、LAN 内の PC をスマホから Wake-on-LAN で起動・監視する中継ツール(開発中)。

- 設定不要: 送信元 NIC とブロードキャストアドレスは自動で求める
- 起動中の機器を ARP や tailnet の端末一覧から見つけて、選ぶだけで登録
- 複数台対応、TCP ポートでの起動確認
- Rust 製の単一バイナリ。Windows サービスとして常駐する想定

状態: 設計段階。`docs/prototype/wol_server.py` は動作確認済みの個人用試作(参考)。

## ライセンス

MIT
