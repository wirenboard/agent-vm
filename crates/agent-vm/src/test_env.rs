//! Process-global serialization point for tests that touch the
//! environment.
//!
//! `cargo test` runs a binary's tests in parallel *threads of one
//! process*, while `setenv(3)` mutates a table shared by all of them —
//! which is why `std::env::set_var` is `unsafe` in Rust 2024. Two
//! failure modes have actually been observed in this crate:
//!
//!  * A test that sets a var races a sibling that *reads* it
//!    (`pull::tests` and `AGENT_VM_INSECURE_REGISTRY`, failing roughly
//!    one run in three).
//!  * A test that sets a var races a sibling that *spawns a process*:
//!    `Command::spawn` walks `environ` to build the child's
//!    environment, so a concurrent `setenv` can make the spawn fail
//!    outright (`msb_install::tests` and `MSB_PATH`).
//!
//! Both are fixed the same way: every test that mutates the
//! environment, reads a var another test mutates, or spawns a child
//! process holds [`guard`] for its duration. One lock for the whole
//! binary, because the hazard is process-wide — a per-module lock would
//! not stop the cross-module spawn race.

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the process-wide environment lock for the rest of the scope.
///
/// Poisoning is ignored: a panic in one test has already failed that
/// test, and refusing the lock afterwards would cascade into unrelated
/// failures that hide the original one.
pub fn guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
