const video = document.getElementById("clip");
const statusLine = document.getElementById("status");
const source = new MediaSource();
const url = URL.createObjectURL(source);
async function loadTrack(path) {
    const response = await fetch(path);
    if (!response.ok) throw new Error("Track fetch failed");
    const bytes = new Uint8Array(await response.arrayBuffer());
    // The first Cluster separates the initialization and media data in these fixed clips.
    const split = bytes.findIndex((v, i) => v === 0x1f && bytes[i + 1] === 0x43
        && bytes[i + 2] === 0xb6 && bytes[i + 3] === 0x75);
    if (split < 1) throw new Error("Missing media cluster");
    return [bytes.subarray(0, split), bytes.subarray(split)];
}

function append(buffer, bytes) {
    return new Promise((resolve, reject) => {
        const failed = () => reject(new Error("Track append failed"));
        buffer.addEventListener("error", failed, {once: true});
        buffer.addEventListener("updateend", () => {
            buffer.removeEventListener("error", failed);
            resolve();
        }, {once: true});
        buffer.appendBuffer(bytes);
    });
}

source.addEventListener("sourceopen", async () => {
    try {
        const picture = await loadTrack("../media/red-blue.webm");
        const sound = await loadTrack("../media/tone-opus.webm");
        const videoType = 'video/webm; codecs="vp9"';
        const audioType = 'audio/webm; codecs="opus"';
        const audioFirst = video.hasAttribute("data-audio-first");
        const first = source.addSourceBuffer(audioFirst ? audioType : videoType);
        const second = source.addSourceBuffer(audioFirst ? videoType : audioType);
        const [v, a] = audioFirst ? [second, first] : [first, second];
        await append(a, sound[0]);
        await append(v, picture[0]);
        await append(v, picture[1]);
        if (!v.buffered.length || a.buffered.length) throw new Error("Video range leaked into audio");
        await append(a, sound[1]);
        if (!a.buffered.length || v.buffered.end(0) <= a.buffered.end(0))
            throw new Error("Independent track durations missing");
        const ranges = [v, a].map(b => b.buffered.end(0).toFixed(3));
        statusLine.style.color = "white";
        source.endOfStream();
        statusLine.textContent = "Video and audio buffered " + ranges.join(" / ");
    } catch (error) {
        statusLine.textContent = error.message;
    }
}, {once: true});
const soundTest = video.hasAttribute("data-resume-on-seek");
video.addEventListener("seeked", async () => {
    statusLine.textContent = "Seeked to " + video.currentTime.toFixed(1);
    if (soundTest && Math.abs(video.currentTime - 0.5) < 0.05) {
        try {
            // Give the frame readback gate a paused sample at the seek target.
            await new Promise(resolve => setTimeout(resolve, 650));
            await video.play();
            statusLine.textContent = "Playing after seek";
        } catch (error) {
            statusLine.textContent = "Play failed: " + error.name;
        }
    }
});
if (soundTest) {
    const enable = document.createElement("button");
    enable.textContent = "Load with sound";
    statusLine.after(enable);
    enable.addEventListener("click", () => {
        enable.remove();
        video.muted = false;
        video.src = url;
    }, {once: true});
} else {
    video.src = url;
}
window.addEventListener("pagehide", () => URL.revokeObjectURL(url), {once: true});
