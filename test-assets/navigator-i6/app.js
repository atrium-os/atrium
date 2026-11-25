// I6.0 fixture: each check appends one line to #checks and is POSTed to
// /report, so the fixture server logs the outcome without a screenshot.
const checks = document.getElementById("checks");
async function report(name, outcome, detail = "") {
    const li = document.createElement("li");
    li.id = "check-" + name;
    li.className = outcome;
    li.textContent = `${name}: ${outcome}${detail ? " (" + detail + ")" : ""}`;
    checks.appendChild(li);
    try {
        await fetch("/report", { method: "POST", headers: { "content-type": "application/json" },
            body: JSON.stringify({ name, outcome, detail }) });
    } catch (_) { /* the server log is a convenience; the page shows the line */ }
}
// A check that never settles (a permission prompt nobody answers) reports
// "timeout" after 5 s instead of holding back every check after it.
const within = p => Promise.race([p, new Promise((_, no) => setTimeout(() => no(new Error("timeout")), 5000))]);
const step = async (name, fn) => {
    try { const detail = await within(Promise.resolve().then(fn)); await report(name, "pass", detail || ""); }
    catch (e) { await report(name, "fail", String(e && e.message || e)); }
};

// A module script ran at all.
await report("module", "pass");

// D12: the third-party script must not have run.
await report("third-party", window.thirdPartyRan ? "fail" : "refused",
    window.thirdPartyRan ? "cdn.thirdparty.example ran" : "");

// D9: the cookie session survives Stop and a new engine.
await step("session", async () => {
    const r = await fetch("/api/me");
    const me = await r.json();
    document.getElementById("who").textContent = me.user ? "Signed in as " + me.user : "Not signed in";
    return me.user ? "signed in as " + me.user : "not signed in";
});

// D10: localStorage and IndexedDB persist per (persona, origin).
await step("local-storage", async () => {
    const n = Number(localStorage.getItem("visits") || "0") + 1;
    localStorage.setItem("visits", String(n));
    return "visit " + n;
});
function db() {
    return new Promise((ok, no) => {
        const r = indexedDB.open("i6", 1);
        r.onupgradeneeded = () => r.result.createObjectStore("items", { autoIncrement: true });
        r.onsuccess = () => ok(r.result);
        r.onerror = () => no(r.error);
    });
}
async function items() {
    const d = await db();
    return new Promise((ok, no) => {
        const r = d.transaction("items").objectStore("items").getAll();
        r.onsuccess = () => ok(r.result);
        r.onerror = () => no(r.error);
    });
}
async function show() {
    const list = document.getElementById("items");
    list.replaceChildren(...(await items()).map(t => Object.assign(document.createElement("li"), { textContent: t })));
}
await step("indexeddb", async () => { await show(); return (await items()).length + " stored"; });
document.getElementById("add").addEventListener("submit", async e => {
    e.preventDefault();
    const text = document.getElementById("item").value;
    const d = await db();
    await new Promise((ok, no) => {
        const t = d.transaction("items", "readwrite");
        t.objectStore("items").add(text);
        t.oncomplete = ok; t.onerror = () => no(t.error);
    });
    // Script-made same-origin POST (no confirmation in I6).
    await step("script-post", async () => {
        const r = await fetch("/api/items", { method: "POST", headers: { "content-type": "application/json" },
            body: JSON.stringify({ text }) });
        if (!r.ok) throw new Error("HTTP " + r.status);
        return (await r.json()).count + " on the server";
    });
    await show();
});

// Memory (I6.6): Grow holds 64 MiB more in touched 8 MiB chunks, Release
// drops every reference, so the next collection and purge can return it.
let held = [];
const showHeld = () => { document.getElementById("held").textContent = held.length * 8 + " MiB held"; };
document.getElementById("grow").addEventListener("click", () => {
    for (let i = 0; i < 8; i++) held.push(new Uint8Array(8 * 1024 * 1024).fill(1));
    showHeld();
});
document.getElementById("release").addEventListener("click", () => { held = []; showHeld(); });

// I6.7 second part: the Cache API is kept too. One entry a run, under a
// path of its own, so the count is how many runs this site has had.
await step("cache-api", async () => {
    const cache = await caches.open("i6");
    const n = (await cache.keys()).length + 1;
    await cache.put("/cached/" + n, new Response("run " + n, { headers: { "content-type": "text/plain" } }));
    const back = await (await cache.match("/cached/" + n)).text();
    if (back !== "run " + n) throw new Error("read back " + back);
    return n + " cached";
});

// I6.7: navigatord's 50 MiB a site. Fill puts 30 MiB in one transaction;
// the second Fill must abort with QuotaExceededError.
async function write(change) {
    const d = await db();
    return new Promise((ok, no) => {
        const t = d.transaction("items", "readwrite");
        change(t.objectStore("items"));
        t.oncomplete = ok;
        t.onabort = () => no(t.error || new Error("aborted"));
    });
}
document.getElementById("fill").addEventListener("click", () =>
    step("indexeddb-fill", async () => { await write(s => s.add(new Uint8Array(30 * 1024 * 1024).fill(7))); return "30 MiB kept"; }));
document.getElementById("empty").addEventListener("click", () =>
    step("indexeddb-empty", async () => { await write(s => s.clear()); await show(); return "emptied"; }));

// D12: a sibling host on the registrable domain is reachable.
await step("same-site-api", async () => {
    const r = await fetch("https://api.app.example/status", { credentials: "include" });
    if (!r.ok) throw new Error("HTTP " + r.status);
    return (await r.json()).status;
});

// File input through Scrinium's picker.
document.getElementById("file").addEventListener("change", async e => {
    const f = e.target.files[0];
    if (!f) return;
    await step("file-input", async () => {
        const text = await f.text();
        document.getElementById("file-out").textContent = `${f.name}, ${f.size} bytes`;
        return `${f.name} ${f.size} bytes, starts ${JSON.stringify(text.slice(0, 8))}`;
    });
});

// Downloads: one made by script, one from the server.
const blob = new Blob(["I6 fixture notes\n"], { type: "text/plain" });
document.getElementById("dl-blob").href = URL.createObjectURL(blob);

// Navigation: a pushState route stays in the engine as a trail entry.
document.getElementById("route").addEventListener("click", e => {
    e.preventDefault();
    history.pushState({ route: 1 }, "", "/route/1");
    report("push-state", "pass", location.pathname);
});
window.addEventListener("popstate", () => report("pop-state", "pass", location.pathname));
// window.open to another origin is proposed to navigatord, never opened here.
document.getElementById("popup").addEventListener("click", () => {
    const w = window.open("https://elsewhere.example/", "_blank");
    report("window-open", w ? "fail" : "refused", w ? "the engine opened a window" : "");
});

// The payment frame reports through postMessage once it runs in its own jail.
window.addEventListener("message", e => {
    if (e.origin === "https://pay.example") report("frame", "pass", String(e.data));
});

// Refused APIs must say so, not vanish.
await step("notifications", async () => {
    if (!("Notification" in window)) return "absent";
    const p = await Notification.requestPermission();
    if (p === "granted") throw new Error("granted");
    return p;
});
await step("geolocation", () => new Promise((ok, no) => {
    if (!navigator.geolocation) return ok("absent");
    navigator.geolocation.getCurrentPosition(() => no(new Error("granted")), e => ok("denied " + e.code));
}));
await step("service-worker", async () => {
    if (!("serviceWorker" in navigator)) return "absent";
    try { await navigator.serviceWorker.register("/sw.js"); throw new Error("registered"); }
    catch (e) { if (e.message === "registered") throw e; return "refused"; }
});
await step("webgl", async () => {
    const gl = document.createElement("canvas").getContext("webgl");
    if (gl) throw new Error("a WebGL context exists");
    return "none";
});
await step("wasm", async () => {
    // (module (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))
    const bytes = new Uint8Array([0,97,115,109,1,0,0,0,1,7,1,96,2,127,127,1,127,3,2,1,0,7,7,1,3,97,100,100,0,0,10,9,1,7,0,32,0,32,1,106,11]);
    const { instance } = await WebAssembly.instantiate(bytes);
    const v = instance.exports.add(2, 3);
    if (v !== 5) throw new Error("add gave " + v);
    return "interpreter ran";
});
await report("done", "pass");
