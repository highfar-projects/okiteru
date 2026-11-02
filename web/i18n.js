"use strict";

// 画面の文言を言語ごとの辞書(/lang/<言語>.json)から引く、小さな仕組み。
// 言語を足す手順は README の「言語を足す」を参照。
(function () {
  const LANGUAGES = { ja: "日本語", en: "English" };
  const SOURCE = "ja"; // 辞書の原本。ほかの言語に無い文言は、これで補う
  const DEFAULT = "en"; // ブラウザの言語がどれにも当たらないとき
  const STORAGE_KEY = "okiteru.lang";

  function stored() {
    try {
      const value = localStorage.getItem(STORAGE_KEY);
      return value in LANGUAGES ? value : null;
    } catch {
      return null;
    }
  }

  function detect() {
    for (const tag of navigator.languages ?? [navigator.language]) {
      const base = String(tag).toLowerCase().split("-")[0];
      if (base in LANGUAGES) return base;
    }
    return DEFAULT;
  }

  const lang = stored() ?? detect();
  let messages = {};
  let source = {};

  function fill(text, params) {
    return text.replace(/\{(\w+)\}/g, (whole, key) => (params && key in params ? String(params[key]) : whole));
  }

  // t("key", { name: "x" })。辞書に無いキーは、原本の文言、それも無ければキーそのものを返す。
  function t(key, params) {
    return fill(messages[key] ?? source[key] ?? key, params);
  }

  // サーバーのエラー応答から、選んだ言語の文を作る。知らない code のときは、サーバーの文をそのまま使う。
  function errorMessage(data, status) {
    const params = { ...(data?.params ?? {}) };
    if (data?.code === "agent_failed") {
      const reason = data.params?.reason;
      if (reason === "connect" || reason === "send" || reason === "response") return t(`err.agent_${reason}`, params);
      if (reason && `err.${reason}` in messages) {
        if (params.action) params.action = t(`power.${params.action}`);
        return t(`err.${reason}`, params);
      }
    }
    if (data?.code && `err.${data.code}` in messages) return t(`err.${data.code}`, params);
    return data?.error || t("err.generic", { status });
  }

  function apply(root) {
    for (const node of root.querySelectorAll("[data-i18n]")) {
      const text = t(node.dataset.i18n);
      const code = node.dataset.i18nCode;
      if (code === undefined) {
        node.textContent = text;
        continue;
      }
      // {code} の位置に <code> を差し込む
      const [before, after = ""] = text.split("{code}");
      const mono = document.createElement("code");
      mono.textContent = code;
      node.replaceChildren(before, mono, after);
    }
    for (const node of root.querySelectorAll("[data-i18n-placeholder]")) node.placeholder = t(node.dataset.i18nPlaceholder);
    for (const node of root.querySelectorAll("[data-i18n-label]")) node.label = t(node.dataset.i18nLabel);
    for (const node of root.querySelectorAll("[data-i18n-aria]")) node.setAttribute("aria-label", t(node.dataset.i18nAria));
    document.documentElement.lang = lang;
  }

  async function load(name) {
    const res = await fetch(`/lang/${name}.json`);
    if (!res.ok) throw new Error(`lang ${name}: ${res.status}`);
    return res.json();
  }

  function setLang(next) {
    if (!(next in LANGUAGES)) return;
    try {
      localStorage.setItem(STORAGE_KEY, next);
    } catch {
      // 保存できなくても、この回の表示は切り替わる
    }
    location.reload();
  }

  function mountSwitcher() {
    const select = document.getElementById("lang");
    if (!select) return;
    for (const [code, label] of Object.entries(LANGUAGES)) select.append(new Option(label, code));
    select.value = lang;
    select.addEventListener("change", () => setLang(select.value));
  }

  async function boot() {
    try {
      [source, messages] = await Promise.all([load(SOURCE), lang === SOURCE ? null : load(lang)]);
      if (!messages) messages = source;
    } catch {
      // 辞書を読めなくても、キーのまま動かし続ける(画面が真っ白にならないように)
    }
    window.i18n = { t, errorMessage, lang, languages: LANGUAGES };
    apply(document);
    mountSwitcher();
    const app = document.createElement("script");
    app.src = "/app.js";
    document.body.append(app);
  }

  boot();
})();
