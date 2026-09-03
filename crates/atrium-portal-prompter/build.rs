//! Re-emit the native-library paths published by the optics `-sys` crates via
//! their `links` metadata. `rustc-link-arg` does not propagate across crates,
//! so the terminal binary must re-emit the paths itself — this keeps the
//! prompter correct for both system-installed optics libraries and the
//! opt-in local optics override (`.cargo/optics-local.toml`).

fn main() {
    // Keep the optics paths selected by the -sys crates ahead of unrelated
    // search paths introduced by other dependencies: the linker must not
    // pick a stale system copy of liblens/libflux over the chosen one.
    for var in ["DEP_IRIS_RPATHS", "DEP_LENS_RPATHS", "DEP_FLUX_RPATHS"] {
        if let Ok(rpaths) = std::env::var(var) {
            for dir in rpaths.split(';').filter(|s| !s.is_empty()) {
                println!("cargo:rustc-link-search=native={dir}");
            }
        }
    }

    let mut emitted_dtags = false;
    for var in ["DEP_IRIS_RPATHS", "DEP_LENS_RPATHS", "DEP_FLUX_RPATHS"] {
        if let Ok(rpaths) = std::env::var(var) {
            if !emitted_dtags {
                // DT_RPATH (not DT_RUNPATH) so the search also covers
                // transitive NEEDED libs (liblens is libiris's NEEDED, not
                // ours).
                println!("cargo:rustc-link-arg=-Wl,--disable-new-dtags");
                emitted_dtags = true;
            }
            for dir in rpaths.split(';').filter(|s| !s.is_empty()) {
                println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
            }
        }
    }
}
