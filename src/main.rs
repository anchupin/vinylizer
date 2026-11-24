#[cfg(any(target_os = "macos", vinyl_check))]
#[macro_use]
mod ffi;
#[cfg(any(target_os = "macos", vinyl_check))]
mod app;
#[cfg(any(target_os = "macos", vinyl_check))]
mod au;
#[cfg(any(target_os = "macos", vinyl_check))]
mod cf;
#[cfg(any(target_os = "macos", vinyl_check))]
mod engine;
#[cfg(any(target_os = "macos", vinyl_check))]
mod hal;
#[cfg(any(target_os = "macos", vinyl_check))]
mod pipeline;
#[cfg(any(target_os = "macos", vinyl_check))]
mod ui;

fn main() {
    #[cfg(any(target_os = "macos", vinyl_check))]
    std::process::exit(app::run());
    #[cfg(not(any(target_os = "macos", vinyl_check)))]
    {
        eprintln!("vinylizer requires macOS 14.2+ (Core Audio process taps)");
        std::process::exit(1);
    }
}
