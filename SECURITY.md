# セキュリティ

## 脆弱性の報告

脆弱性を見つけたときは、公開の Issue ではなく、GitHub の
[Private vulnerability reporting](https://github.com/highfar-projects/okiteru/security/advisories/new)
から非公開で報告してください。

## 前提

okiteru は、Tailscale の tailnet の中で使うことを前提にしています。認証は Tailscale に任せており、
tailnet に参加している端末(と localhost)からの操作を受け付けます。インターネットに公開しないでください
(`tailscale funnel` やポート転送は使わないでください)。
操作できる端末を絞りたいときは、Tailscale の ACL を使ってください(中継は 8765、agent は 8766)。

守っていることは [README](README.md#安全面で守っていること) にあります。
