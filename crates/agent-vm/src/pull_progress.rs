//! Render microsandbox `PullProgress` events as a single byte-weighted bar.
//!
//! The bar is sized in *bytes* rather than steps so the ETA reflects real
//! work remaining. Total bytes = compressed-download bytes (known from
//! `Resolved.total_download_bytes`) + uncompressed-materialize bytes
//! (learned per layer from `LayerMaterializeProgress.total_bytes` as each
//! layer starts being decompressed).
//!
//! Until we know the materialize sizes, we start with a 4× heuristic
//! estimate (download:materialize ≈ 1:3 for typical Debian-based images)
//! so the bar length doesn't visibly leap each time a new layer starts
//! materializing. As real sizes come in, the bar length is corrected.
//! Indicatif's ETA tracks bytes-per-second across both phases.

use std::collections::HashMap;

use indicatif::{ProgressBar, ProgressStyle};
use microsandbox::sandbox::{PullProgress, PullProgressHandle};

/// Heuristic: compressed download is roughly 1/(1+RATIO) of total bytes.
/// 3 fits Debian-slim + Node.js images we've measured (350 MiB download,
/// ~1.1 GiB materialized).
const ESTIMATED_DECOMPRESS_RATIO: u64 = 3;

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
    /// Bytes downloaded per layer so we can compute deltas regardless of
    /// whether microsandbox emitted any `LayerDownloadProgress` events.
    dl_bytes: HashMap<usize, u64>,
    /// Bytes materialized per layer for the same reason.
    mat_bytes: HashMap<usize, u64>,
    /// Per-layer materialize *total* once we learn it from the first
    /// `LayerMaterializeProgress` for that layer.
    mat_totals: HashMap<usize, u64>,
    /// True if we've replaced the initial estimate with sum-of-known.
    known_materialize_sum: u64,
    estimated_materialize: u64,
}

impl State {
    fn start() -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_style(spinner_style());
        bar.set_message("Resolving image manifest");
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        Self {
            bar,
            layer_count: 0,
            dl_bytes: HashMap::new(),
            mat_bytes: HashMap::new(),
            mat_totals: HashMap::new(),
            known_materialize_sum: 0,
            estimated_materialize: 0,
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
                let dl_total = total_download_bytes.unwrap_or(0);
                self.estimated_materialize = dl_total.saturating_mul(ESTIMATED_DECOMPRESS_RATIO);
                let initial_len = dl_total.saturating_add(self.estimated_materialize);
                self.bar.set_length(initial_len.max(1));
                self.bar.set_style(bar_style());
                self.bar.set_message(format!("Downloading {layer_count} layers"));
                // Keep steady_tick on so the {spinner} in the template
                // animates between byte updates during slow materialize.
            }

            PullProgress::LayerDownloadProgress {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                let prev = self.dl_bytes.insert(layer_index, downloaded_bytes).unwrap_or(0);
                self.bar.inc(downloaded_bytes.saturating_sub(prev));
            }

            PullProgress::LayerDownloadComplete {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                let prev = self.dl_bytes.insert(layer_index, downloaded_bytes).unwrap_or(0);
                self.bar.inc(downloaded_bytes.saturating_sub(prev));
                let done = self.dl_bytes.len();
                if done < self.layer_count {
                    self.bar.set_message(format!(
                        "Downloading layer {}/{}",
                        done + 1,
                        self.layer_count
                    ));
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

            PullProgress::LayerMaterializeProgress {
                layer_index,
                bytes_read,
                total_bytes,
            } => {
                // First time we see this layer's total, fold it into the
                // bar length: subtract our per-layer share of the estimate,
                // add the real number.
                if self.mat_totals.insert(layer_index, total_bytes).is_none() {
                    self.known_materialize_sum =
                        self.known_materialize_sum.saturating_add(total_bytes);
                    self.refresh_length();
                }
                let prev = self.mat_bytes.insert(layer_index, bytes_read).unwrap_or(0);
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
                // Ensure the bar reflects the full layer total even if the
                // last Progress event was a few KiB short.
                if let Some(&total) = self.mat_totals.get(&layer_index) {
                    let prev = self.mat_bytes.insert(layer_index, total).unwrap_or(0);
                    self.bar.inc(total.saturating_sub(prev));
                }
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

    /// Reshape the bar length: download_total (already in length) was
    /// estimate_materialize, replace with what we now know plus the
    /// estimate for layers we haven't seen yet.
    fn refresh_length(&mut self) {
        let dl_total: u64 = self.dl_bytes.values().sum::<u64>().max(
            self.bar.length().unwrap_or(0).saturating_sub(self.estimated_materialize),
        );
        let unseen_layers = self
            .layer_count
            .saturating_sub(self.mat_totals.len()) as u64;
        let per_layer_estimate = if self.layer_count > 0 {
            self.estimated_materialize / (self.layer_count as u64)
        } else {
            0
        };
        let pending_estimate = unseen_layers.saturating_mul(per_layer_estimate);
        let new_len = dl_total
            .saturating_add(self.known_materialize_sum)
            .saturating_add(pending_estimate);
        self.bar.set_length(new_len.max(1));
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
