// navigatord puts this script first in a page run live (navigator-islands.md
// I6.4). The engine has no backend for these APIs, so it is what refuses
// them; this script answers each at once (a page waiting on a permission
// prompt nobody will see just stops) and reports it, so the chip's popover
// can say what the page was refused. It also hands a file the page's script
// made (`<a download>` on a blob: or data: address) to navigatord, which
// asks the person. A page that gets around this script is still refused,
// only not reported.
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
})();
