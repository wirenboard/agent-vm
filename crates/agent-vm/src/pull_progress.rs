//! Render microsandbox `PullProgress` events as a two-phase line.
//!
//! Microsandbox's pull is two very different workloads back-to-back:
//!
//! 1. **Download.** Layers are fetched from the registry. For a local
//!    registry this is over in ~1 s and microsandbox often skips
//!    `LayerDownloadProgress` events entirely, so any bar we draw here
//!    jumps from 0 to "done" in a single tick. The rate computed from that
//!    burst is meaningless (and poisons the rate for the whole rest of the
//!    pull, since indicatif's estimator averages it in).
//!
//! 2. **Materialize.** Each compressed layer is decompressed into an
//!    EROFS image. This is CPU-bound and slow — minutes for the Node.js
//!    layer alone — and fires `LayerMaterializeProgress` events every
//!    ~256 KiB so the rate is genuinely informative.
//!
//! So we draw a spinner with text for phase 1 (no bar, no rate), then
//! finish_and_clear it and start a fresh bar sized in materialize bytes
//! for phase 2. The bar's rate and ETA only ever see materialize samples
//! and stay honest.

use std::collections::HashMap;

use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
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
    phase: Phase,
    layer_count: usize,
    dl_total_bytes: u64,
    dl_done_bytes: HashMap<usize, u64>,
    mat_totals: HashMap<usize, u64>,
    mat_done: HashMap<usize, u64>,
    /// Sum of known materialize totals; the bar length tracks this.
    known_materialize_sum: u64,
}

enum Phase {
    ResolveOrDownload,
    Materialize,
    Stitch,
}

impl State {
    fn start() -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_style(spinner_style());
        bar.set_message("Resolving image manifest");
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        Self {
            bar,
            phase: Phase::ResolveOrDownload,
            layer_count: 0,
            dl_total_bytes: 0,
            dl_done_bytes: HashMap::new(),
            mat_totals: HashMap::new(),
            mat_done: HashMap::new(),
            known_materialize_sum: 0,
        }
    }

    fn handle(&mut self, event: PullProgress) {
        match event {
            PullProgress::Resolving { .. } => {}

            PullProgress::Resolved {
                layer_count,
                total_download_bytes,
                ..
            } => {
                self.layer_count = layer_count;
                self.dl_total_bytes = total_download_bytes.unwrap_or(0);
                self.set_download_message();
            }

            PullProgress::LayerDownloadProgress {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                self.dl_done_bytes.insert(layer_index, downloaded_bytes);
                self.set_download_message();
            }

            PullProgress::LayerDownloadComplete {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                self.dl_done_bytes.insert(layer_index, downloaded_bytes);
                self.set_download_message();
            }

            PullProgress::LayerDownloadVerifying { layer_index, .. } => {
                self.bar.set_message(format!(
                    "Verifying layer {}/{}",
                    layer_index + 1,
                    self.layer_count
                ));
            }

            PullProgress::LayerMaterializeStarted { layer_index, .. } => {
                self.enter_materialize_phase();
                self.bar.set_message(format!(
                    "Materializing layer {}/{}",
                    layer_index + 1,
                    self.layer_count
                ));
            }

            PullProgress::LayerMaterializeProgress {
                layer_index,
                bytes_read,
                total_bytes,
            } => {
                self.enter_materialize_phase();
                if self.mat_totals.insert(layer_index, total_bytes).is_none() {
                    self.known_materialize_sum =
                        self.known_materialize_sum.saturating_add(total_bytes);
                    self.bar.set_length(self.known_materialize_sum.max(1));
                }
                let prev = self.mat_done.insert(layer_index, bytes_read).unwrap_or(0);
                self.bar.inc(bytes_read.saturating_sub(prev));
            }

            PullProgress::LayerMaterializeWriting { layer_index } => {
                self.bar.set_message(format!(
                    "Writing layer {}/{}",
                    layer_index + 1,
                    self.layer_count
                ));
            }

            PullProgress::LayerMaterializeComplete { layer_index, .. } => {
                if let Some(&total) = self.mat_totals.get(&layer_index) {
                    let prev = self.mat_done.insert(layer_index, total).unwrap_or(0);
                    self.bar.inc(total.saturating_sub(prev));
                }
            }

            PullProgress::StitchMergingTrees { .. } => {
                self.enter_stitch_phase();
                self.bar.set_message("Stitching rootfs");
            }
            PullProgress::StitchWritingFsmeta => {
                self.enter_stitch_phase();
                self.bar.set_message("Writing fsmeta");
            }
            PullProgress::StitchWritingVmdk => {
                self.enter_stitch_phase();
                self.bar.set_message("Writing VMDK descriptor");
            }
            PullProgress::StitchComplete | PullProgress::Complete { .. } => {
                self.enter_stitch_phase();
                self.bar.set_message("Rootfs ready");
            }
        }
    }

    fn set_download_message(&mut self) {
        if !matches!(self.phase, Phase::ResolveOrDownload) {
            return;
        }
        let downloaded: u64 = self.dl_done_bytes.values().sum();
        let msg = if self.layer_count == 0 {
            "Resolving image manifest".to_string()
        } else if self.dl_total_bytes > 0 {
            format!(
                "Downloading {} of {} ({}/{} layers)",
                HumanBytes(downloaded),
                HumanBytes(self.dl_total_bytes),
                self.dl_done_bytes.len(),
                self.layer_count,
            )
        } else {
            format!(
                "Downloading {} ({}/{} layers)",
                HumanBytes(downloaded),
                self.dl_done_bytes.len(),
                self.layer_count,
            )
        };
        self.bar.set_message(msg);
    }

    fn enter_materialize_phase(&mut self) {
        if matches!(self.phase, Phase::Materialize | Phase::Stitch) {
            return;
        }
        self.phase = Phase::Materialize;
        // Wipe the download spinner and replace it with a fresh bar so
        // indicatif's rate/elapsed/ETA estimator starts measuring purely
        // materialize bytes.
        self.bar.finish_and_clear();
        let bar = ProgressBar::new(1); // grows as layer totals arrive
        bar.set_style(bar_style());
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        bar.set_message("Materializing rootfs");
        self.bar = bar;
    }

    fn enter_stitch_phase(&mut self) {
        if matches!(self.phase, Phase::Stitch) {
            return;
        }
        self.phase = Phase::Stitch;
        self.bar.finish_and_clear();
        let bar = ProgressBar::new_spinner();
        bar.set_style(spinner_style());
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        self.bar = bar;
    }

    fn finish(&mut self) {
        self.bar.finish_and_clear();
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap()
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} {bar:25.cyan/blue} {bytes:>9}/{total_bytes:<9} {binary_bytes_per_sec:>11}  eta {eta:>4}  {msg}",
    )
    .unwrap()
    .progress_chars("##-")
}
