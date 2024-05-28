use gl_generator::{Api, Fallbacks, Profile, Registry, StructGenerator};

fn main() -> anyhow::Result<()> {
    let out_dir = std::env::var("OUT_DIR")?;
    let out_dir = std::path::Path::new(&out_dir);
    let target = std::env::var("TARGET").unwrap();
    if target.contains("linux")
        || target.contains("dragonfly")
        || target.contains("freebsd")
        || target.contains("netbsd")
        || target.contains("openbsd")
    {
        let mut file = std::fs::File::create(out_dir.join("egl_bindings.rs")).unwrap();
        let reg = Registry::new(
            Api::Egl,
            (1, 5),
            Profile::Core,
            Fallbacks::All,
            [
                "EGL_ANDROID_native_fence_sync",
                "EGL_KHR_platform_x11",
                "EGL_EXT_device_base",
                "EGL_EXT_device_drm",
                "EGL_EXT_device_drm_render_node",
                "EGL_EXT_device_query",
                "EGL_KHR_fence_sync",
                "EGL_EXT_image_dma_buf_import",
                "EGL_EXT_image_dma_buf_import_modifiers",
            ],
        );

        reg.write_bindings(StructGenerator, &mut file).unwrap();
        let mut file = std::fs::File::create(out_dir.join("gl_bindings.rs")).unwrap();

        Registry::new(
            Api::Gl,
            (3, 3),
            Profile::Core,
            Fallbacks::All,
            ["GL_EXT_EGL_image_storage"],
        )
        .write_bindings(StructGenerator, &mut file)
        .unwrap();
    }
    Ok(())
}
