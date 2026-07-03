//! Embed a Windows application manifest that requests administrator elevation
//! (needed to read the NTFS MFT / USN journal) and enables per-monitor DPI.

fn main() {
    #[cfg(windows)]
    {
        use embed_manifest::manifest::ExecutionLevel;
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(
            new_manifest("WinSearch")
                .requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("failed to embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
