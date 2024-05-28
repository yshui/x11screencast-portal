use std::os::unix::fs::symlink;

#[derive(Debug)]
struct ParseCallbacks;

impl bindgen::callbacks::ParseCallbacks for ParseCallbacks {
    fn enum_variant_name(
        &self,
        enum_name: Option<&str>,
        original_variant_name: &str,
        _variant_value: bindgen::callbacks::EnumVariantValue,
    ) -> Option<String> {
        let enum_name = enum_name?
            .trim_start_matches("enum ")
            .trim_end_matches("_t");
        if original_variant_name
            .to_ascii_lowercase()
            .starts_with(enum_name)
        {
            Some(original_variant_name[enum_name.len() + 1..].to_string())
        } else {
            None
        }
    }
}

fn main() -> anyhow::Result<()> {
    let pixman = pkg_config::probe_library("pixman-1")?;
    let libxcb = pkg_config::probe_library("xcb")?;
    let egl = pkg_config::probe_library("egl")?;
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")?;
    let manifest_dir = std::path::Path::new(&manifest_dir);
    let out_dir = std::env::var("OUT_DIR")?;
    let out_dir = std::path::Path::new(&out_dir);

    println!(
        "cargo:rustc-link-search=native={}",
        egl.link_paths[0].display()
    );

    let bindings = bindgen::Builder::default()
        .header("picom.h")
        .clang_arg(format!("-I{}", manifest_dir.display()))
        .clang_args(
            pixman
                .include_paths
                .iter()
                .map(|p| format!("-I{}", p.display())),
        )
        .clang_args(
            libxcb
                .include_paths
                .iter()
                .map(|p| format!("-I{}", p.display())),
        )
        .allowlist_function("picom_api_get_interfaces")
        .allowlist_function("backend_register")
        .opaque_type("image_handle")
        .newtype_enum(".*")
        .parse_callbacks(Box::new(ParseCallbacks))
        .generate()?;
    bindings.write_to_file(out_dir.join("bindings.rs"))?;

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=picom");

    Ok(())
}
