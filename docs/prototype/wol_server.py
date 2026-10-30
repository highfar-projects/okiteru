"""スマホから Tailscale 経由でメインマシンを WOL 起動するための中継サーバー。

常時稼働マシンで実行する。Python 3.8 以降の標準ライブラリだけで動く。
スマホのブラウザで http://<常時稼働マシンの Tailscale IP>:8765/ を開いて使う。
"""
import ipaddress
import json
import socket
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ---- 設定 ----
TARGET_MAC = "AA:BB:CC:DD:EE:FF"  # メインマシンの有線 NIC の MAC アドレス
BROADCAST = "192.168.1.255"       # LAN のブロードキャストアドレス
LAN_IP = ""                       # 常時稼働マシンの LAN 側 IP。空なら送信元 NIC は OS 任せ
TARGET_IP = "192.168.1.10"        # メインマシンの LAN IP(起動確認に使う)
CHECK_PORT = 3389                 # 起動確認に使う TCP ポート。RDP を受けないなら 445 など
LISTEN_PORT = 8765

# Tailscale と自分自身からのアクセスだけ受け付ける
ALLOWED = [ipaddress.ip_network(n) for n in ("100.64.0.0/10", "127.0.0.0/8")]

PAGE = """<!doctype html>
<html lang="ja">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="apple-mobile-web-app-capable" content="yes">
<link rel="manifest" href="/manifest.json">
<title>メインマシン起動</title>
<style>
  :root { --bg: #f4f4f5; --fg: #18181b; --card: #fff; --accent: #2563eb; --muted: #71717a; }
  @media (prefers-color-scheme: dark) {
    :root { --bg: #18181b; --fg: #f4f4f5; --card: #27272a; --accent: #3b82f6; --muted: #a1a1aa; }
  }
  body { margin: 0; min-height: 100vh; display: grid; place-items: center;
         background: var(--bg); color: var(--fg); font-family: system-ui, sans-serif; }
  main { width: min(360px, calc(100% - 32px)); background: var(--card); border-radius: 16px;
         padding: 32px 24px; text-align: center; }
  h1 { font-size: 1.2rem; margin: 0 0 24px; }
  #state { font-size: 1.1rem; margin-bottom: 24px; }
  .dot { display: inline-block; width: 10px; height: 10px; border-radius: 50%;
         margin-right: 8px; background: var(--muted); }
  .up .dot { background: #16a34a; }
  button { width: 100%; padding: 18px; font-size: 1.1rem; border: 0; border-radius: 12px;
           background: var(--accent); color: #fff; }
  button:disabled { opacity: .5; }
  #msg { margin-top: 16px; min-height: 1.2em; color: var(--muted); font-size: .9rem; }
</style>
</head>
<body>
<main>
  <h1>メインマシン</h1>
  <div id="state"><span class="dot"></span><span id="label">確認中…</span></div>
  <button id="wake">起動する</button>
  <div id="msg"></div>
</main>
<script>
const label = document.getElementById("label");
const state = document.getElementById("state");
const btn = document.getElementById("wake");
const msg = document.getElementById("msg");
let waitUntil = 0;

async function refresh() {
  try {
    const r = await fetch("/status", { cache: "no-store" });
    const { up } = await r.json();
    state.classList.toggle("up", up);
    if (up) {
      label.textContent = "起動しています";
      waitUntil = 0;
      msg.textContent = "";
    } else {
      label.textContent = Date.now() < waitUntil ? "起動待ち…" : "停止しています";
      if (waitUntil && Date.now() >= waitUntil) {
        msg.textContent = "3 分以内に起動を確認できませんでした";
        waitUntil = 0;
      }
    }
    btn.disabled = up || Date.now() < waitUntil;
  } catch {
    label.textContent = "中継サーバーに接続できません";
  }
}

btn.onclick = async () => {
  btn.disabled = true;
  try {
    const r = await fetch("/wake", { method: "POST" });
    if (!r.ok) throw new Error(await r.text());
    waitUntil = Date.now() + 180000;
    msg.textContent = "マジックパケットを送信しました";
  } catch (e) {
    msg.textContent = "送信に失敗しました: " + e.message;
    btn.disabled = false;
  }
  refresh();
};

refresh();
setInterval(refresh, 3000);
</script>
</body>
</html>
"""

MANIFEST = {
    "name": "メインマシン起動",
    "short_name": "WOL",
    "start_url": "/",
    "display": "standalone",
}


def send_magic_packet():
    mac = bytes.fromhex(TARGET_MAC.replace(":", "").replace("-", ""))
    if len(mac) != 6:
        raise ValueError("TARGET_MAC が不正です")
    packet = b"\xff" * 6 + mac * 16
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        if LAN_IP:
            s.bind((LAN_IP, 0))
        for _ in range(3):
            s.sendto(packet, (BROADCAST, 9))


def is_up():
    try:
        with socket.create_connection((TARGET_IP, CHECK_PORT), timeout=1):
            return True
    except OSError:
        return False


class Handler(BaseHTTPRequestHandler):
    def allowed(self):
        client = ipaddress.ip_address(self.client_address[0])
        if any(client in net for net in ALLOWED):
            return True
        self.send_error(403)
        return False

    def reply(self, status, body, content_type):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if not self.allowed():
            return
        if self.path == "/":
            self.reply(200, PAGE, "text/html; charset=utf-8")
        elif self.path == "/status":
            self.reply(200, json.dumps({"up": is_up()}), "application/json")
        elif self.path == "/manifest.json":
            self.reply(200, json.dumps(MANIFEST, ensure_ascii=False), "application/manifest+json")
        else:
            self.send_error(404)

    def do_POST(self):
        if not self.allowed():
            return
        if self.path != "/wake":
            self.send_error(404)
            return
        try:
            send_magic_packet()
        except (OSError, ValueError) as e:
            self.reply(500, str(e), "text/plain; charset=utf-8")
            return
        self.reply(200, json.dumps({"sent": True}), "application/json")

    # pythonw で動かすと stderr が無いので、ログは出さない
    def log_message(self, format, *args):
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", LISTEN_PORT), Handler).serve_forever()
