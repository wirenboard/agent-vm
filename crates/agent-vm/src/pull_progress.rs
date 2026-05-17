//! Render microsandbox `PullProgress` events as indicatif bars on stderr.
//!
//! microsandbox emits per-layer download, materialize, and stitch events.
//! We collapse them into a small set of bars/spinners:
//!
//! - A spinner before the manifest is resolved (we don't yet know layer
//!   count or total size).
//! - One overall download bar in bytes, summing across all layers, as soon
//!   as we know `total_download_bytes`.
//! - A second bar for "materializing" (the EROFS rebuild step) sized by
//!   layer count.
//! - A spinner for the stitching tail end (fsmeta + VMDK writes).
//!
//! Per-layer bars are deliberately omitted — the OCI image has many small
//! layers and a wall of bars is louder than useful for our case.

use std::collections::HashMap;

use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressStyle};
use microsandbox::sandbox::{PullProgress, PullProgressHandle};

/// Drive the progress UI to completion. Returns when the channel closes
/// (i.e. when microsandbox is done pulling, successfully or not).
pub async fn render(mut handle: PullProgressHandle) {
    let mp = MultiProgress::new();
    let mut state = State::new(&mp);

    while let Some(event) = handle.recv().await {
        state.handle(event);
    }

    state.finish();
}

struct State<'a> {
    mp: &'a MultiProgress,
    resolve: Option<ProgressBar>,
    download: Option<ProgressBar>,
    materialize: Option<ProgressBar>,
    stitch: Option<ProgressBar>,
    /// Per-layer downloaded bytes so we can update the overall bar without
    /// double-counting.
    layer_bytes: HashMap<usize, u64>,
}

impl<'a> State<'a> {
    fn new(mp: &'a MultiProgress) -> Self {
        let resolve = mp.add(spinner("Resolving image manifest"));
        Self {
            mp,
            resolve: Some(resolve),
            download: None,
            materialize: None,
            stitch: None,
            layer_bytes: HashMap::new(),
        }
    }

    fn handle(&mut self, event: PullProgress) {
        match event {
            PullProgress::Resolving { .. } => { /* spinner already showing */ }

            PullProgress::Resolved {
                layer_count,
                total_download_bytes,
                ..
            } => {
                if let Some(b) = self.resolve.take() {
                    b.finish_with_message("Manifest resolved");
                }
                let dl = match total_download_bytes {
                    Some(total) => {
                        let b = self.mp.add(ProgressBar::new(total));
                        b.set_style(bytes_bar_style());
                        b.set_message(format!("Downloading {layer_count} layers"));
                        b
                    }
                    None => spinner_in(self.mp, format!("Downloading {layer_count} layers")),
                };
                self.download = Some(dl);

                let mat = self.mp.add(ProgressBar::new(layer_count as u64));
                mat.set_style(layer_bar_style());
                mat.set_message("Materializing rootfs layers");
                self.materialize = Some(mat);
            }

            PullProgress::LayerDownloadProgress {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                let delta = downloaded_bytes
                    .saturating_sub(*self.layer_bytes.get(&layer_index).unwrap_or(&0));
                self.layer_bytes.insert(layer_index, downloaded_bytes);
                if let Some(b) = &self.download {
                    b.inc(delta);
                }
            }

            PullProgress::LayerDownloadComplete {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                // Make sure the bar accounts for the final byte count even
                // if the last progress event didn't deliver it.
                let prev = self.layer_bytes.insert(layer_index, downloaded_bytes);
                if let Some(b) = &self.download
                    && let Some(prev) = prev
                {
                    b.inc(downloaded_bytes.saturating_sub(prev));
                }
            }

            PullProgress::LayerDownloadVerifying { .. } => {}

            PullProgress::LayerMaterializeStarted { .. }
            | PullProgress::LayerMaterializeProgress { .. }
            | PullProgress::LayerMaterializeWriting { .. } => {}

            PullProgress::LayerMaterializeComplete { .. } => {
                if let Some(b) = &self.materialize {
                    b.inc(1);
                }
            }

            PullProgress::StitchMergingTrees { .. } => {
                if let Some(b) = self.download.take() {
                    b.finish_with_message(format!(
                        "Downloaded {}",
                        HumanBytes(b.position())
                    ));
                }
                if let Some(b) = self.materialize.take() {
                    b.finish_with_message("Layers materialized");
                }
                let s = spinner_in(self.mp, "Stitching rootfs");
                self.stitch = Some(s);
            }

            PullProgress::StitchWritingFsmeta => {
                if let Some(b) = &self.stitch {
                    b.set_message("Writing fsmeta");
                }
            }
            PullProgress::StitchWritingVmdk => {
                if let Some(b) = &self.stitch {
                    b.set_message("Writing VMDK descriptor");
                }
            }
            PullProgress::StitchComplete => {
                if let Some(b) = self.stitch.take() {
                    b.finish_with_message("Rootfs ready");
                }
            }

            PullProgress::Complete { .. } => {
                // All bars already finished above; nothing to do.
            }
        }
    }

    fn finish(&mut self) {
        for b in [
            self.resolve.take(),
            self.download.take(),
            self.materialize.take(),
            self.stitch.take(),
        ]
        .into_iter()
        .flatten()
        {
            b.finish_and_clear();
        }
    }
}

fn spinner(msg: impl Into<std::borrow::Cow<'static, str>>) -> ProgressBar {
    let b = ProgressBar::new_spinner();
    b.set_style(spinner_style());
    b.set_message(msg);
    b.enable_steady_tick(std::time::Duration::from_millis(120));
    b
}

fn spinner_in(mp: &MultiProgress, msg: impl Into<std::borrow::Cow<'static, str>>) -> ProgressBar {
    let b = mp.add(ProgressBar::new_spinner());
    b.set_style(spinner_style());
    b.set_message(msg);
    b.enable_steady_tick(std::time::Duration::from_millis(120));
    b
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}").unwrap()
}

fn bytes_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{bar:30.cyan/blue} {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>10} eta {eta:>3} {msg}",
    )
    .unwrap()
    .progress_chars("##-")
}

fn layer_bar_style() -> ProgressStyle {
    ProgressStyle::with_template("{bar:30.green/blue} {pos:>3}/{len:<3} layers {msg}")
        .unwrap()
        .progress_chars("##-")
}
