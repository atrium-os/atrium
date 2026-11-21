// Diagnostic fixture for the first browser-to-broker hop. Only the relying
// party can finish sign-in after checking the returned assertion.
const next = document.getElementById("next");
const statusLine = document.getElementById("status");
next.textContent = "Use passkey";
next.addEventListener("click", async () => {
    next.disabled = true;
    try {
        const assertion = await navigator.credentials.get({
            publicKey: {
                challenge: crypto.getRandomValues(new Uint8Array(32)),
                rpId: location.hostname,
                userVerification: "required",
                timeout: 30000
            }
        });
        statusLine.textContent = assertion?.type === "public-key"
            ? "Assertion returned" : "No public-key assertion";
    } catch (error) {
        statusLine.textContent = "Passkey failed: " + error.name;
    }
});
