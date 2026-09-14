//! This helper only loads NetBurrow's DLL into a matching, same-user x86 game instance.
#[cfg(all(windows, target_arch = "x86"))]
#[path = "../../netburrow-core/src/process.rs"]
mod process;

#[path = "../../netburrow-core/src/diagnostics.rs"]
mod diagnostics;
#[cfg(all(windows, target_arch = "x86"))]
mod inject;

fn main() {
    diagnostics::init("injector");
    #[cfg(all(windows, target_arch = "x86"))]
    if let Err(error) = inject::run() {
        diagnostics::record("ERROR", "inject", &error.to_string());
        eprintln!("NetBurrow helper: {error}");
        std::process::exit(1);
    }
    #[cfg(not(all(windows, target_arch = "x86")))]
    {
        eprintln!("Build this helper for i686-pc-windows-msvc.");
        std::process::exit(1);
    }
}
