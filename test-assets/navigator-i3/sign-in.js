const next = document.getElementById("next");
const statusLine = document.getElementById("status");
next.textContent = "Use passkey";
function base64url(buffer) {
    return btoa(String.fromCharCode(...new Uint8Array(buffer)))
        .replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
const abortTest = document.createElement("button");
abortTest.type = "button";
abortTest.textContent = "Test page abort";
next.after(abortTest);
abortTest.addEventListener("click", () => signIn(true));
next.addEventListener("click", () => signIn(false));

async function signIn(abortPending) {
    abortTest.disabled = true;
    const controller = new AbortController();
    const timer = abortPending ? setTimeout(() => controller.abort(), 5000) : undefined;
    next.disabled = true;
    try {
        const challenge = Uint8Array.from(atob("__CHALLENGE__".replace(/-/g, "+").replace(/_/g, "/")), c => c.charCodeAt(0));
        const credential = await navigator.credentials.get({signal: controller.signal, publicKey: {
            challenge, rpId: location.hostname, userVerification: "required", timeout: 60000
        }});
        if (!(credential instanceof PublicKeyCredential)
            || !(credential.response instanceof AuthenticatorAssertionResponse)) throw new Error("Invalid assertion");
        const response = credential.response;
        const assertion = {
            type: credential.type, rawId: base64url(credential.rawId),
            clientDataJSON: base64url(response.clientDataJSON),
            authenticatorData: base64url(response.authenticatorData),
            signature: base64url(response.signature), userHandle: base64url(response.userHandle)
        };
        statusLine.textContent = "Verifying passkey";
        await fetch("/atrium-fixture/continue", { method: "POST", credentials: "same-origin",
            body: new URLSearchParams({assertion: JSON.stringify(assertion)}) });
    } catch (error) {
        statusLine.textContent = "Passkey failed: " + error.name;
        next.disabled = false;
        abortTest.disabled = false;
    } finally {
        clearTimeout(timer);
    }
}
