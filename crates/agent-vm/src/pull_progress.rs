//! Render microsandbox `PullProgress` events as a per-layer multi-bar
//! display that mirrors `msb pull` exactly.
//!
//! Layout:
//!
//! ```text
//!    ⠙ Pulling      ghcr.io/... (5 layers)
//!      layer 1/5  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  12.0 MiB/12.0 MiB  ✓
//!      layer 2/5  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  48.3 MiB/48.3 MiB  materializing
//!      ...
//! ```
//!
//! One header spinner + one bar per layer. Each bar transitions through
//! three styles: magenta (downloading) → blue (materializing) → checkmark
//! (done). No ETA, no aggregate bar — same as `msb pull`.
//!
//! Kept in lockstep with `vendor/microsandbox/crates/cli/lib/ui.rs`
//! (`PullProgressDisplay`); if msb changes its template, mirror it here.

use std::io::IsTerminal;
use std::time::Duration;

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use microsandbox::sandbox::{PullProgress, PullProgressHandle};

const BRAILLE_TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Drive the progress UI to completion. Returns when the channel closes.
pub async fn render(mut handle: PullProgressHandle) {
    let mut display = PullProgressDisplay::new();
    while let Some(event) = handle.recv().await {
        display.handle_event(event);
    }
    display.finish();
}

/// Await the spawned render task, logging any panic so a template typo
/// (or any other panic inside `render`) is visible rather than silently
/// eaten by `JoinHandle::await.ok()`. Cancellation is silent — callers
/// that abort the task on an error path are intentionally tearing it
/// down. Use this from every `agent-vm` command that spawns `render`.
pub async fn await_render(handle: tokio::task::JoinHandle<()>) {
    match handle.await {
        Ok(()) => {}
        Err(e) if e.is_cancelled() => {}
        Err(e) => eprintln!("warn: progress render task failed: {e}"),
    }
}

struct PullProgressDisplay {
    mp: MultiProgress,
    header: ProgressBar,
    layer_bars: Vec<ProgressBar>,
    /// Per-layer "has already entered materialize style" flag. Parallel
    /// to `layer_bars`. Lets every materialize-phase event force the
    /// style transition if `LayerMaterializeStarted` was dropped by the
    /// bounded try_send channel (or skipped entirely on the cached-EROFS
    /// fast path where Complete arrives without a prior Started).
    materialize_styled: Vec<bool>,
    reference: String,
    download_style: ProgressStyle,
    materialize_style: ProgressStyle,
    done_style: ProgressStyle,
}

impl PullProgressDisplay {
    fn new() -> Self {
        let is_tty = std::io::stderr().is_terminal();

        let mp = MultiProgress::new();
        if is_tty {
            mp.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
        } else {
            mp.set_draw_target(ProgressDrawTarget::hidden());
        }

        let header = mp.add(ProgressBar::new_spinner());
        header.set_style(
            ProgressStyle::default_spinner()
                .tick_strings(BRAILLE_TICKS)
                .template("   {spinner} {msg}")
                .unwrap(),
        );
        header.set_message(format!("{:<12} ...", "Resolving"));
        if is_tty {
            header.enable_steady_tick(Duration::from_millis(80));
        }

        Self {
            mp,
            header,
            layer_bars: Vec::new(),
            materialize_styled: Vec::new(),
            reference: String::new(),
            download_style: ProgressStyle::default_bar()
                .template(
                    "     {prefix}  {bar:36.magenta/238}  {bytes}/{total_bytes}  {msg:.magenta}",
                )
                .unwrap()
                .progress_chars("━━╌"),
            materialize_style: ProgressStyle::default_bar()
                .template("     {prefix}  {bar:36.blue/238}  {bytes}/{total_bytes}  {msg:.blue}")
                .unwrap()
                .progress_chars("━━╌"),
            done_style: ProgressStyle::default_bar()
                .template("     {prefix}  {msg}")
                .unwrap(),
        }
    }

    fn handle_event(&mut self, event: PullProgress) {
        match event {
            PullProgress::Resolving { reference } => {
                self.reference = reference.to_string();
                self.header
                    .set_message(format!("{:<12} {}...", "Resolving", self.reference));
            }
            PullProgress::Resolved { layer_count, reference, .. } => {
                if self.reference.is_empty() {
                    self.reference = reference.to_string();
                }
                self.header.set_message(format!(
                    "{:<12} {} ({} layer{})",
                    "Pulling",
                    self.reference,
                    layer_count,
                    if layer_count == 1 { "" } else { "s" }
                ));

                let width = layer_count.to_string().len();
                for i in 0..layer_count {
                    let pb = self.mp.add(ProgressBar::new(1));
                    pb.set_style(self.download_style.clone());
                    pb.set_prefix(format!("layer {:>width$}/{layer_count}", i + 1));
                    pb.set_message("downloading");
                    self.layer_bars.push(pb);
                    self.materialize_styled.push(false);
                }
            }
            PullProgress::LayerDownloadProgress {
                layer_index,
                downloaded_bytes,
                total_bytes,
                ..
            } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    if let Some(total) = total_bytes {
                        pb.set_length(total);
                    }
                    pb.set_position(downloaded_bytes);
                }
            }
            PullProgress::LayerDownloadComplete {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_length(downloaded_bytes);
                    pb.set_position(downloaded_bytes);
                }
            }
            PullProgress::LayerDownloadVerifying { layer_index, .. } => {
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_message("verifying");
                }
            }
            PullProgress::LayerMaterializeStarted { layer_index, .. } => {
                self.ensure_materialize_style(layer_index);
            }
            PullProgress::LayerMaterializeProgress {
                layer_index,
                bytes_read,
                total_bytes,
            } => {
                self.ensure_materialize_style(layer_index);
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_length(total_bytes);
                    pb.set_position(bytes_read);
                }
            }
            PullProgress::LayerMaterializeWriting { layer_index } => {
                self.ensure_materialize_style(layer_index);
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_position(pb.length().unwrap_or(0));
                    pb.set_message("writing image");
                }
            }
            PullProgress::LayerMaterializeComplete { layer_index, .. } => {
                self.ensure_materialize_style(layer_index);
                if let Some(pb) = self.layer_bars.get(layer_index) {
                    pb.set_position(pb.length().unwrap_or(0));
                    pb.set_style(self.done_style.clone());
                    pb.set_message(format!("{}", style("✓").green().for_stderr()));
                    pb.tick();
                }
            }
            PullProgress::StitchMergingTrees { layer_count } => {
                self.header.set_message(format!(
                    "{:<12} {} ({} layer{})",
                    "Merging",
                    self.reference,
                    layer_count,
                    if layer_count == 1 { "" } else { "s" }
                ));
            }
            PullProgress::StitchWritingFsmeta => {
                self.header
                    .set_message(format!("{:<12} {}", "Writing fsmeta", self.reference));
            }
            PullProgress::StitchWritingVmdk => {
                self.header
                    .set_message(format!("{:<12} {}", "Writing vmdk", self.reference));
            }
            PullProgress::StitchComplete => {
                self.header
                    .set_message(format!("{:<12} {}", "Stitched", self.reference));
            }
            PullProgress::Complete { .. } => {}
        }
    }

    /// Force the bar at `layer_index` into `materialize_style` if it
    /// hasn't already transitioned. Idempotent. Covers two skipped-event
    /// scenarios: (a) the bounded try_send progress channel dropped
    /// `LayerMaterializeStarted` under load; (b) the per-layer cached-
    /// EROFS fast path in `registry.rs` emits `Complete` with no prior
    /// `Started`. Without this, the bar would stay in download_style
    /// (magenta with the literal "downloading" tag) while materialize
    /// events update its bytes — visibly misleading.
    fn ensure_materialize_style(&mut self, layer_index: usize) {
        if self
            .materialize_styled
            .get(layer_index)
            .copied()
            .unwrap_or(true)
        {
            return;
        }
        let Some(pb) = self.layer_bars.get(layer_index) else {
            return;
        };
        pb.set_style(self.materialize_style.clone());
        pb.set_position(0);
        pb.set_length(1);
        pb.set_message("materializing");
        self.materialize_styled[layer_index] = true;
    }

    fn finish(self) {
        let _ = self.mp.clear();
    }
}
