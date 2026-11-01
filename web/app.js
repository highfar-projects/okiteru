"use strict";

const POLL_MS = 3000;
const WAIT_MS = 180000; // ボタンを押してから、起動・停止を待つ時間
const POWER_LABELS = { hibernate: "休止状態", sleep: "スリープ", shutdown: "シャットダウン" };

const $ = (selector) => document.querySelector(selector);

let devices = [];
let timer = null;
const waitingUntil = new Map(); // id -> 起動待ちの期限
const sleepingUntil = new Map(); // id -> 停止待ちの期限
const notes = new Map(); // id -> カードに出すメッセージ { text, error }
const cards = new Map(); // id -> { li, ...要素 }

async function api(method, path, body) {
  const options = { method, cache: "no-store" };
  if (method !== "GET") {
    options.headers = { "Content-Type": "application/json" };
    options.body = JSON.stringify(body ?? {});
  }
  const res = await fetch(path, options);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `エラーが発生しました (${res.status})`);
  return data;
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function setNote(node, text, isError) {
  node.textContent = text;
  node.classList.toggle("error", Boolean(isError));
}

// ---- 機器の一覧 ----

function createCard(id) {
  const li = el("li", "card");
  const head = el("div", "card-head");
  const name = el("h2");
  const edit = el("button", "", "設定");
  edit.type = "button";
  edit.addEventListener("click", () => openEdit(id));
  head.append(name, edit);

  const sub = el("p", "sub");
  const state = el("div", "state");
  const label = el("span");
  state.append(el("span", "dot"), label);
  const wake = el("button", "wake primary", "起きてください");
  wake.type = "button";
  wake.addEventListener("click", () => wakeDevice(id));
  const sleep = el("button", "sleep", "お休みなさい");
  sleep.type = "button";
  sleep.addEventListener("click", () => sleepDevice(id));
  const note = el("p", "note");
  note.setAttribute("role", "status");

  li.append(head, sub, state, wake, sleep, note);
  return { li, name, sub, state, label, wake, sleep, note };
}

function render() {
  const now = Date.now();
  const seen = new Set();
  const list = $("#devices");

  for (const d of devices) {
    seen.add(d.id);
    let card = cards.get(d.id);
    if (!card) {
      card = createCard(d.id);
      cards.set(d.id, card);
      list.append(card.li);
    }

    const deadline = waitingUntil.get(d.id) ?? 0;
    if (d.up) {
      waitingUntil.delete(d.id);
      notes.delete(d.id);
    } else if (deadline && now >= deadline) {
      waitingUntil.delete(d.id);
      notes.set(d.id, { text: "3 分以内に起動を確認できませんでした", error: true });
    }
    const waiting = !d.up && waitingUntil.has(d.id);

    const sleepDeadline = sleepingUntil.get(d.id) ?? 0;
    if (!d.up) {
      sleepingUntil.delete(d.id);
    } else if (sleepDeadline && now >= sleepDeadline) {
      sleepingUntil.delete(d.id);
      notes.set(d.id, { text: "3 分以内に止まりませんでした。相手の設定を確認してください", error: true });
    }
    const sleeping = d.up && sleepingUntil.has(d.id);

    card.name.textContent = d.name;
    card.sub.textContent = `${d.ip} ・ 確認ポート ${d.check_port}`;
    card.state.classList.toggle("up", d.up);
    card.state.classList.toggle("waiting", waiting || sleeping);
    card.label.textContent = sleeping ? "お休み待ち…" : d.up ? "起きています" : waiting ? "起動待ち…" : "停止しています";
    card.wake.disabled = d.up || waiting;
    card.sleep.hidden = !d.tailnet_ip; // 相手の agent に届く IP が分かっている機器だけ
    card.sleep.disabled = !d.up || sleeping;
    const note = notes.get(d.id);
    setNote(card.note, note ? note.text : "", note?.error);
  }

  for (const [id, card] of cards) {
    if (!seen.has(id)) {
      card.li.remove();
      cards.delete(id);
      waitingUntil.delete(id);
      sleepingUntil.delete(id);
      notes.delete(id);
    }
  }
  $("#empty").hidden = devices.length > 0;
}

async function refresh() {
  try {
    devices = await api("GET", "/api/devices");
    $("#banner").hidden = true;
  } catch (e) {
    const banner = $("#banner");
    banner.textContent = e instanceof TypeError ? "中継サーバーに接続できません" : e.message;
    banner.hidden = false;
  }
  render();
}

async function wakeDevice(id) {
  const card = cards.get(id);
  if (card) card.wake.disabled = true;
  try {
    await api("POST", `/api/devices/${id}/wake`);
    waitingUntil.set(id, Date.now() + WAIT_MS);
    notes.set(id, { text: "マジックパケットを送信しました" });
  } catch (e) {
    notes.set(id, { text: `送信に失敗しました: ${e.message}`, error: true });
  }
  render();
  refresh();
}

async function sleepDevice(id) {
  const d = devices.find((x) => x.id === id);
  if (!d || !confirm(`「${d.name}」を${POWER_LABELS[d.power]}にしますか?`)) return;
  const card = cards.get(id);
  if (card) card.sleep.disabled = true;
  try {
    await api("POST", `/api/devices/${id}/sleep`);
    sleepingUntil.set(id, Date.now() + WAIT_MS);
    notes.set(id, { text: `${POWER_LABELS[d.power]}を頼みました` });
  } catch (e) {
    notes.set(id, { text: `頼めませんでした: ${e.message}`, error: true });
  }
  render();
  refresh();
}

function start() {
  stop();
  refresh();
  timer = setInterval(refresh, POLL_MS);
}

function stop() {
  if (timer !== null) clearInterval(timer);
  timer = null;
}

document.addEventListener("visibilitychange", () => (document.hidden ? stop() : start()));

// ---- 追加 ----

const addDialog = $("#add-dialog");

function openAdd() {
  $("#candidates").replaceChildren();
  setNote($("#scan-note"), "");
  setNote($("#manual-note"), "");
  addDialog.showModal();
}

async function scan() {
  const button = $("#scan");
  const list = $("#candidates");
  button.disabled = true;
  list.replaceChildren();
  setNote($("#scan-note"), "探しています…(数秒かかります)");
  try {
    const found = await api("GET", "/api/candidates");
    if (found.length === 0) {
      setNote(
        $("#scan-note"),
        "見つかりませんでした。対象の PC が起動していて、Tailscale が動いているか確認してください。手で入力して追加することもできます。",
        true,
      );
    } else {
      setNote($("#scan-note"), "");
      for (const c of found) list.append(candidateItem(c));
    }
  } catch (e) {
    setNote($("#scan-note"), `探せませんでした: ${e.message}`, true);
  }
  button.disabled = false;
}

function candidateItem(c) {
  const li = el("li", "candidate");
  const info = el("div");
  info.append(el("strong", "", c.host_name), el("span", "", `${c.ip} ・ ${c.mac} ・ ${c.os}`));
  const add = el("button", "primary", c.registered ? "登録済み" : "追加");
  add.type = "button";
  add.disabled = c.registered;
  add.addEventListener("click", async () => {
    add.disabled = true;
    try {
      const body = { name: c.host_name, mac: c.mac, ip: c.ip };
      if (c.tailnet_ip) body.tailnet_ip = c.tailnet_ip;
      await api("POST", "/api/devices", body);
      add.textContent = "登録済み";
      refresh();
    } catch (e) {
      add.disabled = false;
      setNote($("#scan-note"), e.message, true);
    }
  });
  li.append(info, add);
  return li;
}

$("#manual-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.target;
  const port = form.check_port.value;
  const body = { name: form.name.value, mac: form.mac.value, ip: form.ip.value };
  if (port) body.check_port = Number(port);
  try {
    await api("POST", "/api/devices", body);
    form.reset();
    addDialog.close();
    refresh();
  } catch (e) {
    setNote($("#manual-note"), e.message, true);
  }
});

$("#open-add").addEventListener("click", openAdd);
$("#scan").addEventListener("click", scan);
$("#close-add").addEventListener("click", () => addDialog.close());

// ---- 編集・削除 ----

const editDialog = $("#edit-dialog");
let editingId = null;

function openEdit(id) {
  const d = devices.find((x) => x.id === id);
  if (!d) return;
  editingId = id;
  const form = $("#edit-form");
  form.name.value = d.name;
  form.ip.value = d.ip;
  form.check_port.value = d.check_port;
  form.power.value = d.power;
  form.tailnet_ip.value = d.tailnet_ip ?? "";
  $("#edit-mac").textContent = d.mac;
  setNote($("#edit-note"), "");
  editDialog.showModal();
}

$("#edit-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.target;
  try {
    await api("PATCH", `/api/devices/${editingId}`, {
      name: form.name.value,
      ip: form.ip.value,
      check_port: Number(form.check_port.value),
      power: form.power.value,
      tailnet_ip: form.tailnet_ip.value.trim(),
    });
    editDialog.close();
    refresh();
  } catch (e) {
    setNote($("#edit-note"), e.message, true);
  }
});

$("#delete").addEventListener("click", async () => {
  const d = devices.find((x) => x.id === editingId);
  if (!d || !confirm(`「${d.name}」を削除しますか?`)) return;
  try {
    await api("DELETE", `/api/devices/${editingId}`);
    editDialog.close();
    refresh();
  } catch (e) {
    setNote($("#edit-note"), e.message, true);
  }
});

$("#close-edit").addEventListener("click", () => editDialog.close());

start();
