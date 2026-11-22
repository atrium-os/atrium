const next = document.getElementById("next");
const statusLine = document.getElementById("status");
function decode(text) {
    return Uint8Array.from(atob(text.replace(/-/g, "+").replace(/_/g, "/")), c => c.charCodeAt(0));
}
function encode(buffer) {
    return btoa(String.fromCharCode(...new Uint8Array(buffer))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
async function submit(name, credential, fields) {
    if (attachment && credential.authenticatorAttachment !== attachment) throw new Error("Unexpected authenticator");
    const data = {type: credential.type, rawId: encode(credential.rawId)};
    for (const field of fields) data[field] = encode(credential.response[field]);
    await fetch("/atrium-fixture/continue", {method: "POST", credentials: "same-origin",
        body: new URLSearchParams({[name]: JSON.stringify(data)})});
}
const attachment = "__ATTACHMENT__";
function create(challenge, excludeCredentials) {
    return navigator.credentials.create({publicKey: {
        rp: {name: "Local I3 provider", id: location.hostname},
        user: {id: new TextEncoder().encode("fixture-user"), name: "ada@example.test", displayName: "Ada"},
        challenge: decode(challenge), pubKeyCredParams: [{type: "public-key", alg: -7}], excludeCredentials,
        authenticatorSelection: {authenticatorAttachment: attachment || undefined, residentKey: "required", userVerification: "required"}, timeout: 60000
    }});
}
const duplicate = document.createElement("button");
duplicate.type = "button";
duplicate.textContent = "Try duplicate";
duplicate.addEventListener("click", async () => {
    duplicate.disabled = true;
    next.disabled = true;
    try {
        await create("__EXCLUDE__", [{type: "public-key", id: registered}]);
        statusLine.textContent = "Duplicate unexpectedly created";
    } catch (error) {
        statusLine.textContent = "Duplicate: " + error.name;
        next.disabled = error.name !== "InvalidStateError";
    }
});
let registered;
next.textContent = "Create passkey";
next.addEventListener("click", async () => {
    next.disabled = true;
    try {
        if (!registered) {
            const key = await create("__CREATE__", []);
            if (!(key instanceof PublicKeyCredential) || !(key.response instanceof AuthenticatorAttestationResponse))
                throw new Error("Invalid registration");
            await submit("registration", key, ["clientDataJSON", "attestationObject"]);
            registered = key.rawId;
            next.after(duplicate);
            next.textContent = "Use new passkey";
            statusLine.textContent = "Passkey created";
            next.disabled = false;
        } else {
            duplicate.disabled = true;
            const key = await navigator.credentials.get({publicKey: {challenge: decode("__GET__"), rpId: location.hostname,
                allowCredentials: [{type: "public-key", id: registered}], userVerification: "required", timeout: 60000}});
            if (!(key.response instanceof AuthenticatorAssertionResponse) || encode(key.rawId) !== encode(registered))
                throw new Error("Unexpected credential");
            await submit("assertion", key, ["clientDataJSON", "authenticatorData", "signature", "userHandle"]);
        }
    } catch (error) {
        statusLine.textContent = "Passkey failed: " + error.name;
        next.disabled = false;
    }
});
