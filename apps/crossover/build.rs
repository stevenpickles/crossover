//! Embed the Windows application icon into `crossover.exe` so it carries the
//! Crossover brand in Explorer, the taskbar, and — later — the system tray.
//!
//! Windows-only: on other build hosts this is a no-op (the icon is a Windows PE
//! resource), so tri-OS CI is unaffected. Best-effort — a resource-compiler
//! failure warns rather than breaking the build, since branding is not
//! correctness.

fn main() {
    #[cfg(windows)]
    {
        // Relative to this crate; the shared icon lives at the repo root.
        const ICON: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/branding/crossover.ico"
        );
        println!("cargo:rerun-if-changed=../../assets/branding/crossover.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(ICON);
        if let Err(error) = resource.compile() {
            println!("cargo:warning=embedding the application icon failed: {error}");
        }
    }
}
