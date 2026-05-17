//! Render microsandbox `PullProgress` events as a single indicatif bar.
//!
//! microsandbox emits per-layer events for download (resolve, download,
//! verify), materialize (the slow EROFS rebuild), and stitch (fsmeta +
//! VMDK writes). For interactive use a single bar that always advances is
//! more honest than several stacked bars — especially because microsandbox
//! often elides `LayerDownloadProgress` entirely when the source is fast
//! (e.g. a local registry), leaving a "stuck at 0 B" bar with the old
//! design.
//!
//! New approach: one bar of length `2 * layer_count`. Each layer counts
//! once when its download finishes and once when its materialize finishes.
//! The bar's message text reflects what phase is currently active.

use indicatif::{ProgressBar, ProgressStyle};
use microsandbox::sandbox::{PullProgress, PullProgressHandle};

/// Drive the progress UI to completion. Returns when the channel closes.
pub async fn render(mut handle: PullProgressHandle) {
    let mut state = State::start();
    while let Some(event) = handle.recv().await {
        state.handle(event);
    }
    state.finish();
}

struct State {
    bar: ProgressBar,
    layer_count: usize,
    /// Tracks "this layer's download has been credited" so a Progress
    /// event followed by Complete doesn't double-count.
    downloaded: Vec<bool>,
}

impl State {
    fn start() -> Self {
        // Start as a spinner. We swap to a bar with a real length as soon as
        // the manifest resolves and we know layer_count.
        let bar = ProgressBar::new_spinner();
        bar.set_style(spinner_style());
        bar.set_message("Resolving image manifest");
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        Self {
            bar,
            layer_count: 0,
            downloaded: Vec::new(),
        }
    }

    fn handle(&mut self, event: PullProgress) {
        match event {
            PullProgress::Resolving { .. } => { /* spinner already showing */ }

            PullProgress::Resolved { layer_count, .. } => {
                self.layer_count = layer_count;
                self.downloaded = vec![false; layer_count];
                self.bar.set_length((layer_count as u64) * 2);
                self.bar.set_style(bar_style());
                self.bar.set_message(format!("Downloading {layer_count} layers"));
                // Keep the steady tick on so the spinner in the bar template
                // animates between layer-completes — the materialize step
                // can take ~15s per layer with no other ticks.
            }

            PullProgress::LayerDownloadProgress { .. } => {
                // Skip — we count whole layers, not bytes. The
                // microsandbox event stream often omits these anyway.
            }

            PullProgress::LayerDownloadComplete { layer_index, .. } => {
                if let Some(slot) = self.downloaded.get_mut(layer_index)
                    && !*slot
                {
                    *slot = true;
                    self.bar.inc(1);
                }
                let done = self.downloaded.iter().filter(|d| **d).count();
                if done < self.layer_count {
                    self.bar
                        .set_message(format!("Downloading layer {}/{}", done + 1, self.layer_count));
                }
            }

            PullProgress::LayerDownloadVerifying { layer_index, .. } => {
                self.bar.set_message(format!(
                    "Verifying layer {}/{}",
                    layer_index + 1,
                    self.layer_count
                ));
            }

            PullProgress::LayerMaterializeStarted { layer_index, .. } => {
                self.bar.set_message(format!(
                    "Materializing layer {}/{}",
                    layer_index + 1,
                    self.layer_count
                ));
            }

            PullProgress::LayerMaterializeProgress { .. }
            | PullProgress::LayerMaterializeWriting { .. } => {}

            PullProgress::LayerMaterializeComplete { .. } => {
                self.bar.inc(1);
            }

            PullProgress::StitchMergingTrees { .. } => {
                self.bar.set_message("Stitching rootfs");
            }
            PullProgress::StitchWritingFsmeta => {
                self.bar.set_message("Writing fsmeta");
            }
            PullProgress::StitchWritingVmdk => {
                self.bar.set_message("Writing VMDK descriptor");
            }
            PullProgress::StitchComplete | PullProgress::Complete { .. } => {
                self.bar.set_message("Rootfs ready");
            }
        }
    }

    fn finish(&mut self) {
        // Wipe the bar from the terminal so subsequent banners / agent
        // output start on a clean line.
        self.bar.finish_and_clear();
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap()
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} {bar:30.cyan/blue} {pos:>3}/{len:<3} steps  {wide_msg} eta {eta:>3}",
    )
    .unwrap()
    .progress_chars("##-")
}
