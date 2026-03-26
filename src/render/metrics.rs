use std::time::Instant;

pub struct FrameMetrics {
    frame_start: Instant,
    last_report: Instant,
    pub frame_count: u64,

    // rolling averages (over last N frames)
    frame_times: [u64; 120],
    frame_idx: usize,

    // cumulative
    pub total_cmds: u64,
    pub total_uploads: u64,
    pub total_upload_bytes: u64,
    pub total_dedup_hits: u64,
    pub total_dedup_bytes_saved: u64,
    pub total_frames_rendered: u64,
    pub total_scene_frames: u64,
}

impl FrameMetrics {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            frame_start: now,
            last_report: now,
            frame_count: 0,
            frame_times: [0; 120],
            frame_idx: 0,
            total_cmds: 0,
            total_uploads: 0,
            total_upload_bytes: 0,
            total_dedup_hits: 0,
            total_dedup_bytes_saved: 0,
            total_frames_rendered: 0,
            total_scene_frames: 0,
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame_start = Instant::now();
    }

    pub fn end_frame(&mut self, render_items: u32) {
        let elapsed = self.frame_start.elapsed().as_micros() as u64;
        if render_items > 0 {
            self.frame_times[self.frame_idx] = elapsed;
            self.frame_idx = (self.frame_idx + 1) % 120;
            self.total_scene_frames += 1;
        }
        self.total_frames_rendered += 1;
        self.frame_count += 1;
    }

    pub fn record_cmd(&mut self) {
        self.total_cmds += 1;
    }

    pub fn record_upload(&mut self, bytes: u64) {
        self.total_uploads += 1;
        self.total_upload_bytes += bytes;
    }

    pub fn should_report(&self) -> bool {
        self.last_report.elapsed().as_secs() >= 5
    }

    pub fn mark_reported(&mut self) {
        self.last_report = Instant::now();
    }

    pub fn avg_scene_fps(&self) -> f32 {
        let filled = self.total_scene_frames.min(120) as usize;
        if filled == 0 { return 0.0; }
        let sum: u64 = self.frame_times[..filled].iter().sum();
        let avg_us = sum / filled as u64;
        if avg_us == 0 { return 0.0; }
        1_000_000.0 / avg_us as f32
    }

    pub fn log_summary(&self, cas_blobs: usize, cas_bytes: usize, gc_freed: u64, gc_bytes: u64,
                        tree_size: u32, tree_shared: u32) {
        let fps = self.avg_scene_fps();
        let dedup_pct = if self.total_uploads > 0 {
            (self.total_dedup_hits as f64 / self.total_uploads as f64) * 100.0
        } else {
            0.0
        };

        log::info!("── metrics ──────────────────────────────────");
        log::info!("  scene fps: {:.1}  |  scene frames: {}",
            fps, self.total_scene_frames);
        log::info!("  cmds: {}  |  uploads: {}  |  upload data: {:.1} KB",
            self.total_cmds, self.total_uploads, self.total_upload_bytes as f64 / 1024.0);
        log::info!("  CAS: {} live blobs ({:.1} KB)  |  dedup: {} hits ({:.1}%)",
            cas_blobs, cas_bytes as f64 / 1024.0,
            self.total_dedup_hits, dedup_pct);
        let share_pct = if tree_size > 0 {
            tree_shared as f64 / tree_size as f64 * 100.0
        } else { 0.0 };
        log::info!("  tree: {} nodes, {} shared ({:.0}%)  |  GC: {} freed ({:.1} KB)",
            tree_size, tree_shared, share_pct, gc_freed, gc_bytes as f64 / 1024.0);
        log::info!("─────────────────────────────────────────────");
    }
}
