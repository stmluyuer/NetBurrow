//! SteamNetworking006 replacement transport with native Steam session handling.
//! No network I/O runs in game calls; the existing NetBurrow IPC carries packets.
#[cfg(any(test, all(windows, target_arch = "x86")))]
mod queue;
#[cfg(any(test, all(windows, target_arch = "x86")))]
mod telemetry;

#[cfg(all(windows, target_arch = "x86"))]
#[path = "../../netburrow-core/src/diagnostics.rs"]
mod diagnostics;
#[cfg(all(windows, target_arch = "x86"))]
mod windows;
