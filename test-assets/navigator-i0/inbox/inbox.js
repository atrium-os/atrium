document.getElementById("compose").addEventListener("click", () => {
    document.getElementById("draft").textContent = "New draft";
});
fetch("/api/inbox", { credentials: "same-origin" })
    .then(response => {
        if (!response.ok) throw new Error("Inbox unavailable");
        return response.json();
    })
    .then(message => { document.getElementById("messages").textContent = message.subject; })
    .catch(() => { document.getElementById("messages").textContent = "Fetch failed"; });
