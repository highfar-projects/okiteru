"use strict";

const POLL_MS = 3000;
const WAIT_MS = 180000; // ボタンを押してから、起動・停止を待つ時間
const { t } = window.i18n;
const powerLabel = (power) => t(`power.${power}`);

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
  if (res.status === 401 && !path.startsWith("/api/auth/")) showLogin();
  if (!res.ok) throw new Error(window.i18n.errorMessage(data, res.status));
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

// ---- ログイン(中継で PIN を設定したときだけ) ----

const loginDialog = $("#login-dialog");

function showLogin() {
  if (loginDialog.open) return;
  stop();
  setNote($("#login-note"), "");
  loginDialog.showModal();
  $("#login-form").pin.focus();
}

// ログイン前は閉じられない(Esc も効かせない)
loginDialog.addEventListener("cancel", (event) => event.preventDefault());

$("#login-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.target;
  try {
    await api("POST", "/api/auth/login", { pin: form.pin.value });
    form.reset();
    loginDialog.close();
    $("#logout").hidden = false;
    start();
    refreshNotifyButton();
  } catch (e) {
    setNote($("#login-note"), e.message, true);
    form.pin.select();
  }
});

$("#logout").addEventListener("click", async () => {
  await api("POST", "/api/auth/logout").catch(() => {});
  location.reload();
});

async function checkAuth() {
  try {
    const status = await api("GET", "/api/auth/status");
    $("#logout").hidden = !status.required;
    if (status.required && !status.authenticated) showLogin();
  } catch {
    // 中継に接続できないときは、一覧の取得の失敗として表示される
  }
}

// ---- 確認 ----

const confirmDialog = $("#confirm-dialog");

// アプリ内の確認画面。「やめる」「Esc」「背景のタップ」はどれも false を返す(何も実行しない)。
function askConfirm({ title, detail, warning, yes, strong = false }) {
  if (confirmDialog.open) return Promise.resolve(false);
  $("#confirm-title").textContent = title;
  $("#confirm-detail").textContent = detail;
  $("#confirm-warning").textContent = warning ?? "";
  $("#confirm-warning").hidden = !warning;
  const yesButton = $("#confirm-yes");
  yesButton.textContent = yes;
  yesButton.classList.toggle("strong", strong);
  confirmDialog.returnValue = "";
  return new Promise((resolve) => {
    confirmDialog.addEventListener("close", () => resolve(confirmDialog.returnValue === "yes"), { once: true });
    confirmDialog.showModal();
  });
}

$("#confirm-yes").addEventListener("click", () => confirmDialog.close("yes"));
$("#confirm-no").addEventListener("click", () => confirmDialog.close("no"));
confirmDialog.addEventListener("click", (event) => {
  if (event.target === confirmDialog) confirmDialog.close("no");
});

// ---- 機器の一覧 ----

function createCard(id) {
  const li = el("li", "card");
  const head = el("div", "card-head");
  const name = el("h2");
  const edit = el("button", "", t("card.settings"));
  edit.type = "button";
  edit.addEventListener("click", () => openEdit(id));
  const plan = el("button", "", t("card.schedule"));
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
  const wake = el("button", "wake primary", t("card.wake"));
  wake.type = "button";
  wake.addEventListener("click", () => wakeDevice(id));
  const sleep = el("button", "sleep", t("card.sleep"));
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
      notes.set(d.id, { text: t("note.wake_timeout"), error: true });
    }
    // 中継が起動要求を送り直したあと、上限まで送っても起きなかったとき
    if (!d.up && d.wake?.gave_up && waitingUntil.has(d.id)) {
      waitingUntil.delete(d.id);
      notes.set(d.id, {
        text: t("note.wake_gave_up", { n: d.wake.sent }),
        error: true,
      });
    }
    const waiting = !d.up && waitingUntil.has(d.id);

    const sleepDeadline = sleepingUntil.get(d.id) ?? 0;
    if (!d.up) {
      sleepingUntil.delete(d.id);
    } else if (sleepDeadline && now >= sleepDeadline) {
      sleepingUntil.delete(d.id);
      notes.set(d.id, { text: t("note.sleep_timeout"), error: true });
    }
    const sleeping = d.up && sleepingUntil.has(d.id);

    card.name.textContent = d.name;
    card.sub.textContent = t("card.sub", { ip: d.ip, port: d.check_port });
    const upcoming = nextSchedule(d.id);
    card.next.textContent = upcoming ? t("card.next", { when: formatWhen(upcoming.next) }) : "";
    card.next.hidden = !upcoming;
    card.state.classList.toggle("up", d.up);
    card.state.classList.toggle("waiting", waiting || sleeping);
    card.label.textContent = sleeping
      ? t("state.sleeping")
      : d.up
        ? t("state.up")
        : waiting
          ? d.wake?.sent > 1
            ? t("state.waiting_sent", { n: d.wake.sent })
            : t("state.waiting")
          : t("state.down");
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
    banner.textContent = e instanceof TypeError ? t("banner.offline") : e.message;
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
    notes.set(id, { text: t("note.wake_sent") });
  } catch (e) {
    notes.set(id, { text: t("note.wake_failed", { error: e.message }), error: true });
  }
  render();
  refresh();
}

async function sleepDevice(id) {
  const d = devices.find((x) => x.id === id);
  if (!d) return;
  const shutdown = d.power === "shutdown";
  const ok = await askConfirm({
    title: t("sleep.title", { name: d.name }),
    detail: t("sleep.detail", { power: powerLabel(d.power) }),
    warning: shutdown ? t("sleep.warning_shutdown") : "",
    yes: t("sleep.yes", { power: powerLabel(d.power) }),
    strong: shutdown,
  });
  if (!ok) return;
  const card = cards.get(id);
  if (card) card.sleep.disabled = true;
  try {
    await api("POST", `/api/devices/${id}/sleep`);
    sleepingUntil.set(id, Date.now() + WAIT_MS);
    notes.set(id, { text: t("note.sleep_sent", { power: powerLabel(d.power) }) });
  } catch (e) {
    notes.set(id, { text: t("note.sleep_failed", { error: e.message }), error: true });
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

document.addEventListener("visibilitychange", () => (document.hidden ? stop() : !loginDialog.open && start()));

// ---- 起動予約 ----

const dayName = (i) => t(`day.${i}`);
const scheduleDialog = $("#schedule-dialog");
const scheduleForm = $("#schedule-form");
let scheduleDeviceId = null;

function nextSchedule(deviceId) {
  const upcoming = schedules.filter((s) => s.device_id === deviceId && s.next);
  upcoming.sort((a, b) => a.next.localeCompare(b.next));
  return upcoming[0];
}

// "2026-11-03T07:30" → 日本語: 「11/3(火) 07:30」、英語: 「Tue 11/3 07:30」
function formatWhen(text) {
  const [date, time] = text.split("T");
  const [y, m, d] = date.split("-").map(Number);
  const weekday = dayName((new Date(y, m - 1, d).getDay() + 6) % 7);
  return t("when", { month: m, day: d, weekday, time });
}

function describeSchedule(s) {
  if (s.repeat === "once") return t("sched.describe_once", { date: s.date.replaceAll("-", "/") });
  const days =
    s.days.join("") === "01234" ? t("sched.weekdays") : s.days.length === 7 ? t("sched.everyday") : s.days.map(dayName).join(t("sched.days_sep"));
  return t("sched.describe_weekly", { days });
}

function scheduleItem(s) {
  const li = el("li", s.enabled ? "schedule" : "schedule off");
  const info = el("div");
  info.append(el("strong", "", s.time), el("span", "", describeSchedule(s) + (s.next ? t("sched.next_suffix", { when: formatWhen(s.next) }) : s.enabled ? "" : t("sched.paused_suffix"))));
  const toggle = el("button", "", s.enabled ? t("sched.pause") : t("sched.resume"));
  toggle.type = "button";
  toggle.addEventListener("click", () => changeSchedule(() => api("PATCH", `/api/schedules/${s.id}`, { enabled: !s.enabled })));
  const remove = el("button", "danger", t("delete"));
  remove.type = "button";
  remove.addEventListener("click", async () => {
    const ok = await askConfirm({ title: t("sched.delete_title"), detail: `${s.time} ・ ${describeSchedule(s)}`, yes: t("sched.delete_yes") });
    if (ok) changeSchedule(() => api("DELETE", `/api/schedules/${s.id}`));
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
  $("#schedule-title").textContent = t("sched.title", { name: d.name });
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

// ---- 通知(Web Push) ----

const notifyButton = $("#notify");

function pushUnsupportedReason() {
  if (!window.isSecureContext) {
    return t("notify.https");
  }
  if (!("serviceWorker" in navigator && "PushManager" in window && "Notification" in window)) {
    return t("notify.unsupported");
  }
  return "";
}

function urlBase64ToBytes(text) {
  const padded = text + "=".repeat((4 - (text.length % 4)) % 4);
  const raw = atob(padded.replaceAll("-", "+").replaceAll("_", "/"));
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
}

async function currentSubscription() {
  const reg = await navigator.serviceWorker.getRegistration("/");
  return reg ? reg.pushManager.getSubscription() : null;
}

async function refreshNotifyButton() {
  let on = false;
  if (!pushUnsupportedReason()) {
    try {
      on = Boolean(await currentSubscription()) && Notification.permission === "granted";
    } catch {
      on = false;
    }
  }
  notifyButton.textContent = on ? t("header.notify_on") : t("header.notify_off");
  notifyButton.dataset.on = on ? "1" : "";
}

async function enableNotify() {
  if ((await Notification.requestPermission()) !== "granted") {
    throw new Error(t("notify.denied"));
  }
  await navigator.serviceWorker.register("/sw.js");
  const reg = await navigator.serviceWorker.ready;
  const { key } = await api("GET", "/api/push/key");
  const sub =
    (await reg.pushManager.getSubscription()) ??
    (await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: urlBase64ToBytes(key) }));
  await api("POST", "/api/push/subscribe", sub.toJSON());
  setNote($("#notify-note"), t("notify.enabled"));
}

async function disableNotify() {
  const sub = await currentSubscription();
  if (!sub) return;
  await api("POST", "/api/push/unsubscribe", { endpoint: sub.endpoint });
  await sub.unsubscribe();
  setNote($("#notify-note"), "");
}

notifyButton.addEventListener("click", async () => {
  const reason = pushUnsupportedReason();
  if (reason) {
    setNote($("#notify-note"), reason, true);
    return;
  }
  notifyButton.disabled = true;
  try {
    if (notifyButton.dataset.on) await disableNotify();
    else await enableNotify();
  } catch (e) {
    setNote($("#notify-note"), e.message, true);
  }
  notifyButton.disabled = false;
  await refreshNotifyButton();
});

refreshNotifyButton();

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
  setNote($("#scan-note"), t("scan.searching"));
  try {
    const found = await api("GET", "/api/candidates");
    if (found.length === 0) {
      setNote($("#scan-note"), t("scan.none"), true);
    } else {
      setNote($("#scan-note"), "");
      for (const c of found) list.append(candidateItem(c));
    }
  } catch (e) {
    setNote($("#scan-note"), t("scan.failed", { error: e.message }), true);
  }
  button.disabled = false;
}

function candidateItem(c) {
  const li = el("li", "candidate");
  const info = el("div");
  info.append(el("strong", "", c.host_name), el("span", "", `${c.ip} ・ ${c.mac} ・ ${c.os}`));
  const add = el("button", "primary", c.registered ? t("candidate.registered") : t("candidate.add"));
  add.type = "button";
  add.disabled = c.registered;
  add.addEventListener("click", async () => {
    add.disabled = true;
    try {
      const body = { name: c.host_name, mac: c.mac, ip: c.ip };
      if (c.tailnet_ip) body.tailnet_ip = c.tailnet_ip;
      await api("POST", "/api/devices", body);
      add.textContent = t("candidate.registered");
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
  $("#edit-mac-line").textContent = t("edit.mac", { mac: d.mac });
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
  if (!d) return;
  const ok = await askConfirm({
    title: t("delete.title"),
    detail: t("delete.detail", { name: d.name }),
    warning: t("delete.warning"),
    yes: t("sched.delete_yes"),
  });
  if (!ok) return;
  try {
    await api("DELETE", `/api/devices/${editingId}`);
    editDialog.close();
    refresh();
  } catch (e) {
    setNote($("#edit-note"), e.message, true);
  }
});

$("#close-edit").addEventListener("click", () => editDialog.close());

checkAuth();
start();
