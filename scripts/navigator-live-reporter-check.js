// The I6.4 reporter (navigatord/src/live_reporter.js) against stand-in
// browser APIs: each refused API answered at once and reported once, and a
// script-made download handed over. Run: node scripts/navigator-live-reporter-check.js navigatord/src/live_reporter.js
const fs = require("fs");
const src = fs.readFileSync(process.argv[2], "utf8");
const sent = [];
const listeners = [];
class Anchor { constructor(attrs, connected) { this.attrs = attrs; this.isConnected = connected; }
  getAttribute(k) { return this.attrs[k] ?? null; } hasAttribute(k) { return k in this.attrs; }
  closest() { return this; } click() { sent.push("original-click"); } }
const g = {
  navigator: { geolocation: { getCurrentPosition() { throw "real"; }, watchPosition() { throw "real"; } },
               mediaDevices: { getUserMedia() { throw "real"; } }, serviceWorker: { register() { throw "real"; } },
               clipboard: { readText() { throw "real"; }, read() { throw "real"; } } },
  Notification: { requestPermission() { throw "real"; } },
  fetch: (url, opts) => { sent.push([url, opts && opts.method || "GET"]);
    return Promise.resolve({ blob: () => Promise.resolve({ size: 17, type: "text/plain" }) }); },
  HTMLAnchorElement: Anchor, document: {}, DOMException,
  window: { open() { sent.push("original-open"); return {}; }, print() { sent.push("original-print"); }, addEventListener: (t, f) => listeners.push([t, f]) },
};
g.window.open = g.window.open;
const f = new Function(...Object.keys(g), src);
f(...Object.values(g));
(async () => {
  const assert = require("assert");
  assert.equal(await g.Notification.requestPermission(), "denied");
  await g.Notification.requestPermission();
  let err; g.navigator.geolocation.getCurrentPosition(() => {}, e => err = e);
  await new Promise(r => setTimeout(r, 5));
  assert.equal(err.code, 1);
  await assert.rejects(g.navigator.mediaDevices.getUserMedia({ video: true }), { name: "NotAllowedError" });
  await assert.rejects(g.navigator.serviceWorker.register("/sw.js"), { name: "SecurityError" });
  await assert.rejects(g.navigator.clipboard.readText(), { name: "NotAllowedError" });
  assert.equal(g.window.open("https://elsewhere.example/"), null);
  g.window.print();
  const key = { ctrlKey: true, key: "p", preventDefault() { this.prevented = true; } };
  listeners.find(l => l[0] === "keydown")[1](key);
  assert.ok(key.prevented, "Ctrl-P is Navigator's");
  // A blob download by click, and one by a.click() off the document.
  const ev = { target: new Anchor({ href: "blob:https://app.example/1", download: "notes.txt" }, true), defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } };
  listeners.find(l => l[0] === "click")[1](ev);
  assert.ok(ev.defaultPrevented);
  new Anchor({ href: "blob:https://app.example/2", download: "two.txt" }, false).click.call(new Anchor({ href: "blob:https://app.example/2", download: "two.txt" }, false));
  // A plain link is the page's.
  const plain = { target: new Anchor({ href: "/second.html" }, true), defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } };
  listeners.find(l => l[0] === "click")[1](plain);
  assert.ok(!plain.defaultPrevented);
  await new Promise(r => setTimeout(r, 20));
  const urls = sent.map(s => Array.isArray(s) ? s.join(" ") : s);
  console.log(urls.join("\n"));
  assert.deepEqual(urls.filter(u => u.includes("refused?api=")).map(u => u.split("=")[1].split(" ")[0]),
    ["notifications", "geolocation", "camera", "service-worker", "clipboard-read", "popup"], "each once");
  assert.ok(urls.includes("https://live-page.atrium.invalid/download?name=notes.txt&type=text%2Fplain POST"));
  assert.ok(urls.includes("https://live-page.atrium.invalid/download?name=two.txt&type=text%2Fplain POST"));
  assert.ok(urls.includes("original-open"), "a navigation still goes");
  assert.equal(urls.filter(u => u === "https://live-page.atrium.invalid/print GET").length, 2, "window.print and Ctrl-P");
  console.log("reporter ok");
})().catch(e => { console.error(e); process.exit(1); });
