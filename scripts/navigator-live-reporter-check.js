// The I6.4 reporter (navigatord/src/live_reporter.js) against stand-in
// browser APIs: each refused API answered at once and reported once, a
// script-made download handed over, and (I6.5) cross-origin frames. Run: node scripts/navigator-live-reporter-check.js navigatord/src/live_reporter.js
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
  await frames();
  console.log("reporter ok");
  process.exit(0);
})().catch(e => { console.error(e); process.exit(1); });

// I6.5: a cross-origin frame is reported, never loaded here, and postMessage
// goes through navigatord both ways; in a frame, the page is reachable only
// by postMessage.
async function frames() {
  const assert = require("assert");
  const run = (currentSrc, extra) => {
    const sent = [], events = [], inbox = [];
    class IFrame {
      constructor(src) { this.attrs = { src }; this.isConnected = true; this.loaded = [src]; this.box = { left: 10, top: 20, width: 320, height: 80 }; }
      getAttribute(k) { return this.attrs[k] ?? null; }
      getBoundingClientRect() { return this.box; }
    }
    Object.defineProperty(IFrame.prototype, "src", { configurable: true, get() { return this.attrs.src; }, set(v) { this.attrs.src = v; this.loaded.push(v); } });
    Object.defineProperty(IFrame.prototype, "contentWindow", { configurable: true, get() { return { real: true }; } });
    const frames = [new IFrame("https://pay.example/frame.html"), new IFrame("/same.html")];
    const win = { open() {}, print() {}, addEventListener() {}, devicePixelRatio: 2, requestAnimationFrame: f => setTimeout(f, 1),
                  dispatchEvent: e => events.push(e) };
    const g = {
      navigator: {}, Notification: undefined, DOMException, HTMLAnchorElement: class { click() {} }, URL, Proxy, setInterval, setTimeout,
      MessageEvent: class { constructor(type, init) { if (init.source && !init.source.postMessage) throw new TypeError("source"); Object.assign(this, { type }, init); } },
      HTMLIFrameElement: IFrame, MutationObserver: undefined, location: { origin: "https://app.example" },
      document: { currentScript: { src: currentSrc }, baseURI: "https://app.example/app.html", querySelectorAll: () => frames },
      fetch: (url, opts) => {
        sent.push([url, opts && opts.method || "GET", opts && opts.body]);
        return Promise.resolve({ json: () => Promise.resolve(url.endsWith("/messages") ? inbox.splice(0) : []) });
      },
      window: win, ...extra,
    };
    new Function(...Object.keys(g), src)(...Object.values(g));
    return { g, sent, events, inbox, frames, win };
  };
  // The page.
  const page = run("https://live-page.atrium.invalid/reporter.js");
  await new Promise(r => setTimeout(r, 30));
  const [pay, same] = page.frames;
  assert.deepEqual(pay.loaded, ["https://pay.example/frame.html", "about:blank"], "not loaded in this engine");
  assert.deepEqual(same.loaded, ["/same.html"], "a same-origin frame is the page's");
  assert.equal(pay.src, "https://pay.example/frame.html", "the page still sees its address");
  const at = page.sent.filter(s => s[0].includes("/frame?")).map(s => s[0]);
  assert.deepEqual(at, ["https://live-page.atrium.invalid/frame?id=1&url=https%3A%2F%2Fpay.example%2Fframe.html&x=20&y=40&w=640&h=160"], "once, in device pixels");
  pay.contentWindow.postMessage({ total: 12 }, "https://pay.example/any");
  assert.deepEqual(page.sent.at(-1), ["https://live-page.atrium.invalid/message?to=1&target=https%3A%2F%2Fpay.example", "POST", "{\"total\":12}"]);
  assert.throws(() => pay.contentWindow.postMessage(1, "pay.example"), { name: "SyntaxError" });
  assert.throws(() => pay.contentWindow.postMessage(() => 1, "*"), { name: "DataCloneError" });
  page.inbox.push({ from: 1, origin: "https://pay.example", data: "\"pay.example running\"" }, { from: 1, origin: "https://pay.example", data: "{" });
  await new Promise(r => setTimeout(r, 300));
  assert.equal(page.events.length, 1, "the bad one dropped");
  assert.deepEqual([page.events[0].data, page.events[0].origin, page.events[0].source], ["pay.example running", "https://pay.example", pay.contentWindow]);
  pay.box = { left: 10, top: -30, width: 320, height: 80 };
  pay.isConnected = false;
  await new Promise(r => setTimeout(r, 600));
  assert.ok(page.sent.some(s => s[0] === "https://live-page.atrium.invalid/frame?id=1&gone=1"));
  // The frame.
  const frame = run("https://live-page.atrium.invalid/reporter.js?frame=1", { HTMLIFrameElement: undefined });
  assert.throws(() => frame.win.parent.document, { name: "SecurityError" });
  frame.win.parent.postMessage("pay.example running", "https://app.example");
  assert.deepEqual(frame.sent.at(-1), ["https://live-page.atrium.invalid/message?target=https%3A%2F%2Fapp.example", "POST", "\"pay.example running\""]);
  assert.ok(!frame.sent.some(s => s[0].includes("/frame?")), "a frame reports no frames");
  frame.inbox.push({ from: null, origin: "https://app.example", data: "1" });
  await new Promise(r => setTimeout(r, 300));
  assert.deepEqual([frame.events[0].data, frame.events[0].origin], [1, "https://app.example"]);
  console.log("frames ok");
}
