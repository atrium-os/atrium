const next = document.getElementById("next");
const statusLine = document.getElementById("status");
next.addEventListener("click", async () => {
    next.disabled = true;
    try {
        const response = await fetch("/atrium-fixture/continue", { credentials: "same-origin" });
        if (!response.ok) throw new Error("Continuation refused");
        statusLine.textContent = (await response.json()).message;
    } catch {
        statusLine.textContent = "Try again";
        next.disabled = false;
    }
});
