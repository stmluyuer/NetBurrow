//! NetBurrow's own SteamNetworking006 adapter. No network I/O runs in game calls.
#[cfg(any(test, all(windows, target_arch = "x86")))]
mod queue;

#[cfg(all(windows, target_arch = "x86"))]
#[path = "../../netburrow-core/src/diagnostics.rs"]
mod diagnostics;
#[cfg(all(windows, target_arch = "x86"))]
mod windows;
