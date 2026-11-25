// navigatord puts this script first in a page run live (navigator-islands.md
// I6.4). The engine has no backend for these APIs, so it is what refuses
// them; this script answers each at once (a page waiting on a permission
// prompt nobody will see just stops) and reports it, so the chip's popover
// can say what the page was refused. It also hands a file the page's script
// made (`<a download>` on a blob: or data: address) to navigatord, which
// asks the person, and reports the page's cross-origin frames, which run in
// engines of their own (I6.5). A page that gets around this script is still
// refused, only not reported; a frame it hides from this script is not run.
(() => {
    "use strict";
    const base = "https://live-page.atrium.invalid/";
    const told = new Set();
    const tell = api => {
        if (told.has(api)) return;
        told.add(api);
        try { fetch(base + "refused?api=" + api, { mode: "no-cors", credentials: "omit" }).catch(() => {}); } catch (_) {}
    };
    const refusal = name => new DOMException("Refused in a page run live", name);
    const replace = (object, key, make) => {
        try {
            if (!object || !(key in object)) return;
            const original = object[key];
            Object.defineProperty(object, key, { configurable: true, writable: true, value: make(original) });
        } catch (_) {}
    };
    const nav = navigator;

    if (typeof Notification !== "undefined") {
        replace(Notification, "requestPermission", () => function (callback) {
            tell("notifications");
            if (typeof callback === "function") setTimeout(() => callback("denied"));
            return Promise.resolve("denied");
        });
    }
    if (nav.geolocation) {
        const denied = { code: 1, message: "Refused in a page run live", PERMISSION_DENIED: 1, POSITION_UNAVAILABLE: 2, TIMEOUT: 3 };
        for (const key of ["getCurrentPosition", "watchPosition"]) {
            replace(nav.geolocation, key, () => function (ok, no) {
                tell("geolocation");
                if (typeof no === "function") setTimeout(() => no(denied));
                return 0;
            });
        }
    }
    if (nav.mediaDevices) {
        replace(nav.mediaDevices, "getUserMedia", () => function (constraints) {
            const c = constraints || {};
            if (c.video) tell("camera");
            if (c.audio || !c.video) tell("microphone");
            return Promise.reject(refusal("NotAllowedError"));
        });
        replace(nav.mediaDevices, "getDisplayMedia", () => function () {
            tell("camera");
            return Promise.reject(refusal("NotAllowedError"));
        });
    }
    replace(nav, "requestMIDIAccess", () => function () { tell("midi"); return Promise.reject(refusal("SecurityError")); });
    for (const [object, key, api] of [[nav.usb, "requestDevice", "usb"], [nav.bluetooth, "requestDevice", "bluetooth"],
                                       [nav.serial, "requestPort", "serial"], [nav.hid, "requestDevice", "hid"]]) {
        replace(object, key, () => function () { tell(api); return Promise.reject(refusal("NotFoundError")); });
    }
    if (nav.serviceWorker) {
        replace(nav.serviceWorker, "register", () => function () { tell("service-worker"); return Promise.reject(refusal("SecurityError")); });
    }
    if (typeof PushManager !== "undefined") {
        replace(PushManager.prototype, "subscribe", () => function () { tell("push"); return Promise.reject(refusal("NotAllowedError")); });
    }
    if (typeof SyncManager !== "undefined") {
        replace(SyncManager.prototype, "register", () => function () { tell("background-sync"); return Promise.reject(refusal("NotAllowedError")); });
    }
    if (nav.clipboard) {
        for (const key of ["read", "readText"]) {
            replace(nav.clipboard, key, () => function () { tell("clipboard-read"); return Promise.reject(refusal("NotAllowedError")); });
        }
    }
    // window.open: a navigation the engine proposes still goes (I6.2); a
    // new window never opens here.
    replace(window, "open", original => function (...args) {
        tell("popup");
        try { original.apply(window, args); } catch (_) {}
        return null;
    });

    // Printing prints the page's address through Navigator (its
    // document-lane rendering, Prelum's sheet); Ctrl-P in the page too.
    replace(window, "print", () => function () {
        try { fetch(base + "print", { mode: "no-cors", credentials: "omit" }).catch(() => {}); } catch (_) {}
    });
    window.addEventListener("keydown", e => {
        if ((e.ctrlKey || e.metaKey) && !e.altKey && (e.key === "p" || e.key === "P")) {
            e.preventDefault();
            window.print();
        }
    });

    // A file the page made: its bytes go to navigatord, which asks the
    // person; nothing is saved without their answer.
    const download = a => {
        const href = a.getAttribute("href") || "";
        if (!a.hasAttribute("download") || !/^(blob|data):/i.test(href)) return false;
        fetch(href).then(r => r.blob()).then(b => {
            if (b.size > 32768) { tell("large-download"); return; }
            const q = "download?name=" + encodeURIComponent((a.getAttribute("download") || "").slice(0, 255))
                + "&type=" + encodeURIComponent((b.type || "").slice(0, 128));
            return fetch(base + q, { method: "POST", mode: "no-cors", credentials: "omit",
                headers: { "content-type": "text/plain" }, body: b });
        }).catch(() => {});
        return true;
    };
    // Last, on the way out: a page that handles the click itself (it
    // prevented the default) keeps it.
    window.addEventListener("click", e => {
        const a = e.target && e.target.closest ? e.target.closest("a[download]") : null;
        if (a && !e.defaultPrevented && download(a)) e.preventDefault();
    });
    // `a.click()` on an anchor that is not in the document.
    replace(HTMLAnchorElement.prototype, "click", original => function () {
        if (!this.isConnected && download(this)) return;
        return original.call(this);
    });

    // Cross-origin frames (I6.5, D11). Each runs in its own engine, an
    // island over this one, never in this page: this script tells
    // navigatord where each one is and carries postMessage both ways.
    // navigatord stamps a message with its sender's real origin and
    // delivers it only to the origin it names.
    const script = typeof document !== "undefined" ? document.currentScript : null;
    const inFrame = !!(script && /[?&]frame=1$/.test(script.src || ""));
    const targetOf = t => {
        const s = t !== null && typeof t === "object" ? (t.targetOrigin === undefined ? "/" : String(t.targetOrigin)) : String(t);
        if (s === "*" || s === "/") return s;
        try { return new URL(s).origin; } catch (_) { throw new DOMException("Invalid target origin", "SyntaxError"); }
    };
    const post = (query, data) => {
        let text;
        try { text = JSON.stringify(data); } catch (_) { text = undefined; }
        if (typeof text !== "string" || text.length > 65536) throw new DOMException("The message could not be cloned", "DataCloneError");
        try {
            fetch(base + "message?" + query, { method: "POST", mode: "no-cors", credentials: "omit",
                headers: { "content-type": "text/plain" }, body: text }).catch(() => {});
        } catch (_) {}
    };
    const deliver = (list, sourceOf) => {
        for (const m of list) {
            if (!m || typeof m.data !== "string" || typeof m.origin !== "string") continue;
            let data;
            try { data = JSON.parse(m.data); } catch (_) { continue; }
            let event;
            try { event = new MessageEvent("message", { data, origin: m.origin, source: sourceOf(m.from) }); }
            catch (_) { event = new MessageEvent("message", { data, origin: m.origin }); }
            try { window.dispatchEvent(event); } catch (_) {}
        }
    };
    let polling = null;
    const poll = sourceOf => {
        if (polling !== null) return;
        polling = setInterval(() => {
            try {
                fetch(base + "messages", { credentials: "omit" }).then(r => r.json())
                    .then(list => { if (Array.isArray(list)) deliver(list, sourceOf); }).catch(() => {});
            } catch (_) {}
        }, 250);
    };

    if (inFrame) {
        // The page that holds this frame: reachable only by postMessage.
        const denied = () => { throw new DOMException("Blocked a frame from reaching a cross-origin page", "SecurityError"); };
        const toPage = (data, t) => post("target=" + encodeURIComponent(targetOf(t)), data);
        const page = new Proxy({}, {
            get(_, k) {
                if (k === "postMessage") return toPage;
                if (k === "closed") return false;
                if (typeof k === "symbol" || k === "then") return undefined;
                return denied();
            },
            set: denied, defineProperty: denied, deleteProperty: denied,
        });
        try { Object.defineProperty(window, "parent", { configurable: true, get: () => page }); } catch (_) {}
        poll(() => page);
    } else if (typeof HTMLIFrameElement !== "undefined" && typeof document !== "undefined" && document.querySelectorAll) {
        const tracked = new Map();
        let next = 1;
        const own = location.origin;
        const proto = HTMLIFrameElement.prototype;
        const src = Object.getOwnPropertyDescriptor(proto, "src");
        const view = Object.getOwnPropertyDescriptor(proto, "contentWindow");
        const report = q => { try { fetch(base + "frame?" + q, { mode: "no-cors", credentials: "omit" }).catch(() => {}); } catch (_) {} };
        const addressOf = el => {
            const raw = el.getAttribute("src");
            if (!raw) return null;
            try { return new URL(raw, document.baseURI).href; } catch (_) { return null; }
        };
        const foreign = url => { try { const u = new URL(url); return u.protocol === "https:" && u.origin !== own; } catch (_) { return false; } };
        const scan = () => {
            const ratio = window.devicePixelRatio || 1;
            for (const el of document.querySelectorAll("iframe")) {
                let t = tracked.get(el);
                if (!t) {
                    const url = addressOf(el);
                    if (!url || !foreign(url)) continue;
                    const id = next++;
                    t = { id, url, key: "", window: { postMessage: (data, o) => post("to=" + id + "&target=" + encodeURIComponent(targetOf(o)), data), closed: false } };
                    tracked.set(el, t);
                    // This engine never loads it; its own engine does.
                    try { if (src && src.set) src.set.call(el, "about:blank"); } catch (_) {}
                }
                const r = el.getBoundingClientRect();
                const at = [r.left, r.top, r.width, r.height].map(v => Math.round(v * ratio));
                const key = at.join(",");
                if (key !== t.key) {
                    t.key = key;
                    report("id=" + t.id + "&url=" + encodeURIComponent(t.url) + "&x=" + at[0] + "&y=" + at[1] + "&w=" + at[2] + "&h=" + at[3]);
                }
            }
            for (const [el, t] of tracked) {
                if (!el.isConnected) { tracked.delete(el); report("id=" + t.id + "&gone=1"); }
            }
            if (tracked.size) poll(from => { for (const t of tracked.values()) if (t.id === from) return t.window; return null; });
        };
        let queued = false;
        const soon = () => {
            if (queued) return;
            queued = true;
            (window.requestAnimationFrame || setTimeout)(() => { queued = false; scan(); });
        };
        // The page sees its frame's address and a window to post to.
        try {
            Object.defineProperty(proto, "src", { configurable: true, enumerable: true,
                get() { const t = tracked.get(this); return t ? t.url : src.get.call(this); },
                set(v) {
                    const t = tracked.get(this);
                    if (t) { tracked.delete(this); report("id=" + t.id + "&gone=1"); }
                    src.set.call(this, v);
                    soon();
                } });
            Object.defineProperty(proto, "contentWindow", { configurable: true, enumerable: true,
                get() { const t = tracked.get(this); return t ? t.window : view.get.call(this); } });
        } catch (_) {}
        if (typeof MutationObserver !== "undefined") {
            new MutationObserver(soon).observe(document, { childList: true, subtree: true, attributes: true,
                attributeFilter: ["src", "style", "class", "width", "height", "hidden"] });
        }
        window.addEventListener("scroll", soon, true);
        window.addEventListener("resize", soon);
        setInterval(scan, 500);
        soon();
    }
})();
