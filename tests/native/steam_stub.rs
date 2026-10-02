//! Test-only Steam ABI stand-in. Never copied into the distribution or a game directory.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]
use std::{
    ffi::{CStr, c_char, c_void},
    sync::atomic::{AtomicUsize, Ordering},
};
static OBJECT: AtomicUsize = AtomicUsize::new(0);
static FINDS: AtomicUsize = AtomicUsize::new(0);
static NETWORKING_LOOKUPS: AtomicUsize = AtomicUsize::new(0);
static CALLBACKS: AtomicUsize = AtomicUsize::new(0);
static USER_READY: AtomicUsize = AtomicUsize::new(0);
static USER_CHECKS: AtomicUsize = AtomicUsize::new(0);
static EARLY_USER_CALLS: AtomicUsize = AtomicUsize::new(0);
#[unsafe(no_mangle)]
pub extern "C" fn FixtureSetUserReady() {
    USER_READY.store(1, Ordering::Release);
}
#[unsafe(no_mangle)]
pub extern "C" fn FixtureUserChecks() -> usize {
    USER_CHECKS.load(Ordering::Acquire)
}
#[unsafe(no_mangle)]
pub extern "C" fn FixtureEarlyUserCalls() -> usize {
    EARLY_USER_CALLS.load(Ordering::Acquire)
}
#[unsafe(no_mangle)]
pub extern "C" fn FixtureSetObject(value: usize) {
    OBJECT.store(value, Ordering::Release);
}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_GetHSteamUser() -> i32 {
    USER_CHECKS.fetch_add(1, Ordering::SeqCst);
    USER_READY.load(Ordering::Acquire) as i32
}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_SteamNetworking_v006() -> *mut c_void {
    NETWORKING_LOOKUPS.fetch_add(1, Ordering::SeqCst);
    OBJECT.load(Ordering::Acquire) as *mut c_void
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SteamInternal_FindOrCreateUserInterface(
    _: i32,
    version: *const c_char,
) -> *mut c_void {
    FINDS.fetch_add(1, Ordering::SeqCst);
    if !version.is_null() && CStr::from_ptr(version).to_bytes() == b"SteamNetworking006" {
        NETWORKING_LOOKUPS.fetch_add(1, Ordering::SeqCst);
        OBJECT.load(Ordering::Acquire) as *mut c_void
    } else {
        std::ptr::null_mut()
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn FixtureFindCalls() -> usize {
    FINDS.load(Ordering::SeqCst)
}
#[unsafe(no_mangle)]
pub extern "C" fn FixtureNetworkingCalls() -> usize {
    NETWORKING_LOOKUPS.load(Ordering::SeqCst)
}
#[unsafe(no_mangle)]
pub extern "C" fn FixtureCallbackCalls() -> usize {
    CALLBACKS.load(Ordering::SeqCst)
}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_SteamUser_v023() -> *mut c_void {
    if USER_READY.load(Ordering::Acquire) == 0 {
        EARLY_USER_CALLS.fetch_add(1, Ordering::SeqCst);
        return std::ptr::null_mut();
    }
    1usize as *mut c_void
}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_ISteamUser_GetSteamID(_: *mut c_void) -> u64 {
    101
}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_RegisterCallback(_: *mut c_void, _: i32) {}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_UnregisterCallback(_: *mut c_void) {}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_RunCallbacks() {
    CALLBACKS.fetch_add(1, Ordering::SeqCst);
}
