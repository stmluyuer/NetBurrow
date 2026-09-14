//! Test-only Steam ABI stand-in. Never copied into the distribution or a game directory.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]
use std::{
    ffi::c_void,
    sync::atomic::{AtomicUsize, Ordering},
};
static OBJECT: AtomicUsize = AtomicUsize::new(0);
#[unsafe(no_mangle)]
pub extern "C" fn FixtureSetObject(value: usize) {
    OBJECT.store(value, Ordering::Release);
}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_GetHSteamUser() -> i32 {
    1
}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_SteamNetworking_v006() -> *mut c_void {
    OBJECT.load(Ordering::Acquire) as *mut c_void
}
#[unsafe(no_mangle)]
pub extern "C" fn SteamAPI_SteamUser_v023() -> *mut c_void {
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
pub extern "C" fn SteamAPI_RunCallbacks() {}
