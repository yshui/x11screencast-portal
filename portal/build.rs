fn main() -> anyhow::Result<()> {
    pkg_config::probe_library("x11-xcb")?;
    pkg_config::probe_library("egl")?;

    println!("cargo:rerun-if-changed=build.rs");

    Ok(())
}
