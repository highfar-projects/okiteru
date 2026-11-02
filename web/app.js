"use strict";

const POLL_MS = 3000;
const WAIT_MS = 180000; // ボタンを押してから、起動・停止を待つ時間
const POWER_LABELS = { hibernate: "休止状態", sleep: "スリープ", shutdown: "シャットダウン" };

const $ = (selector) => document.querySelector(selector);

let devices = [];
let schedules = [];
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
  const plan = el("button", "", "予約");
  plan.type = "button";
  plan.addEventListener("click", () => openSchedules(id));
  const buttons = el("div", "buttons");
  buttons.append(plan, edit);
  head.append(name, buttons);

  const sub = el("p", "sub");
  const next = el("p", "next");
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

  li.append(head, sub, next, state, wake, sleep, note);
  return { li, name, sub, next, state, label, wake, sleep, note };
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
      notes.set(d.id, { text: "3 分たっても起きませんでした", error: true });
    }
    // 中継が起動要求を送り直したあと、上限まで送っても起きなかったとき
    if (!d.up && d.wake?.gave_up && waitingUntil.has(d.id)) {
      waitingUntil.delete(d.id);
      notes.set(d.id, {
        text: `起動要求を ${d.wake.sent} 回送りましたが、起きませんでした。画面下の「起動しないときは」を確認してください`,
        error: true,
      });
    }
    const waiting = !d.up && waitingUntil.has(d.id);

    const sleepDeadline = sleepingUntil.get(d.id) ?? 0;
    if (!d.up) {
      sleepingUntil.delete(d.id);
    } else if (sleepDeadline && now >= sleepDeadline) {
      sleepingUntil.delete(d.id);
      notes.set(d.id, { text: "3 分たってもお休みしませんでした。相手の設定を確認してください", error: true });
    }
    const sleeping = d.up && sleepingUntil.has(d.id);

    card.name.textContent = d.name;
    card.sub.textContent = `${d.ip} ・ 確認ポート ${d.check_port}`;
    const upcoming = nextSchedule(d.id);
    card.next.textContent = upcoming ? `次の予約: ${formatWhen(upcoming.next)}` : "";
    card.next.hidden = !upcoming;
    card.state.classList.toggle("up", d.up);
    card.state.classList.toggle("waiting", waiting || sleeping);
    card.label.textContent = sleeping ? "お休みなさい…(お休み待ち)" : d.up ? "起きています" : waiting ? `起きてください…(起きるのを待っています${d.wake?.sent > 1 ? `。${d.wake.sent} 回目を送りました` : ""})` : "お休み中です";
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
    [devices, schedules] = await Promise.all([api("GET", "/api/devices"), api("GET", "/api/schedules")]);
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
    notes.set(id, { text: "起きてくださいと呼びかけました" });
  } catch (e) {
    notes.set(id, { text: `呼びかけに失敗しました: ${e.message}`, error: true });
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
    notes.set(id, { text: `お休みなさいと伝えました(${POWER_LABELS[d.power]})` });
  } catch (e) {
    notes.set(id, { text: `伝えられませんでした: ${e.message}`, error: true });
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

// ---- 起動予約 ----

const DAY_NAMES = ["月", "火", "水", "木", "金", "土", "日"];
const scheduleDialog = $("#schedule-dialog");
const scheduleForm = $("#schedule-form");
let scheduleDeviceId = null;

function nextSchedule(deviceId) {
  const upcoming = schedules.filter((s) => s.device_id === deviceId && s.next);
  upcoming.sort((a, b) => a.next.localeCompare(b.next));
  return upcoming[0];
}

// "2026-11-03T07:30" → 「11/3(火) 07:30」
function formatWhen(text) {
  const [date, time] = text.split("T");
  const [y, m, d] = date.split("-").map(Number);
  const weekday = DAY_NAMES[(new Date(y, m - 1, d).getDay() + 6) % 7];
  return `${m}/${d}(${weekday}) ${time}`;
}

function describeSchedule(s) {
  if (s.repeat === "once") return `${s.date.replaceAll("-", "/")} に一度だけ`;
  const days = s.days.join("") === "01234" ? "平日" : s.days.length === 7 ? "毎日" : s.days.map((d) => DAY_NAMES[d]).join("");
  return `毎週 ${days}`;
}

function scheduleItem(s) {
  const li = el("li", s.enabled ? "schedule" : "schedule off");
  const info = el("div");
  info.append(el("strong", "", s.time), el("span", "", describeSchedule(s) + (s.next ? ` ・ 次は ${formatWhen(s.next)}` : s.enabled ? "" : " ・ 止めています")));
  const toggle = el("button", "", s.enabled ? "止める" : "再開");
  toggle.type = "button";
  toggle.addEventListener("click", () => changeSchedule(() => api("PATCH", `/api/schedules/${s.id}`, { enabled: !s.enabled })));
  const remove = el("button", "danger", "削除");
  remove.type = "button";
  remove.addEventListener("click", () => {
    if (confirm(`${s.time} の予約を削除しますか?`)) changeSchedule(() => api("DELETE", `/api/schedules/${s.id}`));
  });
  const buttons = el("div", "buttons");
  buttons.append(toggle, remove);
  li.append(info, buttons);
  return li;
}

async function changeSchedule(action) {
  try {
    await action();
    setNote($("#schedule-note"), "");
  } catch (e) {
    setNote($("#schedule-note"), e.message, true);
  }
  await refresh();
  renderSchedules();
}

function renderSchedules() {
  const mine = schedules.filter((s) => s.device_id === scheduleDeviceId);
  $("#schedule-list").replaceChildren(...mine.map(scheduleItem));
  $("#schedule-empty").hidden = mine.length > 0;
}

function openSchedules(id) {
  const d = devices.find((x) => x.id === id);
  if (!d) return;
  scheduleDeviceId = id;
  $("#schedule-title").textContent = `「${d.name}」の起動予約`;
  setNote($("#schedule-note"), "");
  renderSchedules();
  scheduleDialog.showModal();
}

function syncRepeat() {
  const once = scheduleForm.repeat.value === "once";
  $("#schedule-days").hidden = once;
  $("#schedule-date-label").hidden = !once;
  scheduleForm.date.required = once;
}

scheduleForm.repeat.addEventListener("change", syncRepeat);

scheduleForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const body = { device_id: scheduleDeviceId, time: scheduleForm.time.value };
  if (scheduleForm.repeat.value === "once") {
    body.date = scheduleForm.date.value;
  } else {
    body.days = [...scheduleForm.querySelectorAll("input[name=day]:checked")].map((c) => Number(c.value));
  }
  await changeSchedule(() => api("POST", "/api/schedules", body));
});

$("#close-schedule").addEventListener("click", () => scheduleDialog.close());

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
