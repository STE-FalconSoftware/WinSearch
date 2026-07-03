//! Embed a Windows application manifest that requests administrator elevation
//! (needed to read the NTFS MFT / USN journal) and enables per-monitor DPI.

fn main() {
    // Set WS_NO_MANIFEST=1 to build without the elevation manifest (used for the
    // no-admin folder mode / for capturing screenshots). Shipping builds embed it.
    #[cfg(windows)]
    if std::env::var_os("WS_NO_MANIFEST").is_none() {
        use embed_manifest::manifest::ExecutionLevel;
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(
            new_manifest("WinSearch")
                .requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("failed to embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WS_NO_MANIFEST");
}
