const video = document.getElementById("clip");
const statusLine = document.getElementById("status");
const source = new MediaSource();
const url = URL.createObjectURL(source);
source.addEventListener("sourceopen", async () => {
    try {
        const response = await fetch("../media/red-blue.webm");
        if (!response.ok) throw new Error("Clip fetch failed");
        const bytes = await response.arrayBuffer();
        const buffer = source.addSourceBuffer('video/webm; codecs="vp9"');
        buffer.addEventListener("error", () => { statusLine.textContent = "Append failed"; }, {once: true});
        buffer.addEventListener("updateend", () => {
            if (buffer.buffered.length === 0) return;
            source.endOfStream();
            statusLine.textContent = "Clip buffered";
        }, {once: true});
        buffer.appendBuffer(bytes);
    } catch (error) {
        statusLine.textContent = error.message;
    }
}, {once: true});
video.addEventListener("seeked", () => {
    statusLine.textContent = "Seeked to " + video.currentTime.toFixed(1);
});
video.src = url;
window.addEventListener("pagehide", () => URL.revokeObjectURL(url), {once: true});
