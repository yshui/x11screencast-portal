use std::{
    cell::{OnceCell, UnsafeCell},
    collections::HashSet,
    os::{
        fd::{AsFd, AsRawFd},
        raw::c_void,
    },
    rc::Rc,
    sync::{
        mpsc::{Receiver, Sender},
        Arc,
    },
};

use anyhow::Context as _;
use cursor::Cursor;
use slotmap::{DefaultKey, SlotMap};
use smallvec::SmallVec;

mod cursor;
mod ffi;
mod pipewire;
mod server;
use drm_fourcc::DrmModifier;
use gl_bindings::{
    egl,
    gl::{
        self,
        types::{GLboolean, GLint, GLuint},
    },
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, PropMode},
};

#[allow(dead_code, non_camel_case_types, non_snake_case)]
mod picom {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

#[link(name = "EGL")]
extern "C" {
    fn eglGetProcAddress(procname: *const std::os::raw::c_char) -> *const std::os::raw::c_void;
}

struct SavedFnPtrs {
    deinit:      unsafe extern "C" fn(*mut picom::backend_base),
    present:     Option<unsafe extern "C" fn(*mut picom::backend_base) -> bool>,
    root_change: Option<unsafe extern "C" fn(*mut picom::backend_base, *mut picom::session)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, bytemuck::NoUninit)]
#[repr(C)]
struct Extent {
    width:  u32,
    height: u32,
}

impl From<x11rb::protocol::xproto::GetGeometryReply> for Extent {
    fn from(r: x11rb::protocol::xproto::GetGeometryReply) -> Self {
        Self { width: r.width as _, height: r.height as _ }
    }
}

#[derive(Clone)]
struct PipewireSender {
    waker: ::pipewire::channel::Sender<()>,
    tx:    Sender<MessagesToPipewire>,
}

struct PipewireSendGuard<'a>(&'a PipewireSender, bool);

impl<'a> Drop for PipewireSendGuard<'a> {
    fn drop(&mut self) {
        if self.1 {
            self.0.waker.send(()).ok();
        }
    }
}

impl<'a> PipewireSendGuard<'a> {
    fn send(&mut self, msg: MessagesToPipewire) -> anyhow::Result<()> {
        self.1 = true;
        self.0.tx.send(msg).map_err(|_| anyhow::anyhow!("send"))
    }

    fn wake(&mut self) { self.1 = true; }
}

impl PipewireSender {
    fn start_send(&self) -> PipewireSendGuard<'_> { PipewireSendGuard(self, false) }

    fn new(waker: ::pipewire::channel::Sender<()>, tx: Sender<MessagesToPipewire>) -> Self {
        Self { waker, tx }
    }
}

const COPY_VS: &std::ffi::CStr = c"
#version 330
#extension GL_ARB_explicit_uniform_location : enable
layout(location = 0) in vec2 pos;
layout(location = 1) in vec2 uv;
out vec2 v_uv;
layout(location = 1)
uniform vec2 offset;
layout(location = 2)
uniform vec2 scale;
void main() {
    gl_Position = vec4((pos + offset) * scale - vec2(1.0, 1.0), 0.0, 1.0);
    v_uv = uv;
}
";
const COPY_FS: &std::ffi::CStr = c"
#version 330
#extension GL_ARB_explicit_uniform_location : enable
in vec2 v_uv;
layout(location = 0)
uniform sampler2D tex;
void main() {
    gl_FragColor = texture(tex, v_uv);
    //gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0);
}
";
struct PluginContext {
    cursor_monitor: cursor::CursorMonitor,
    x11: x11rb::xcb_ffi::XCBConnection,
    root: u32,
    root_size: Extent,

    // We use UnsafeCell here because only one callback can be called at any given time,
    // so we can safely get mutable reference to the context from the `Rc``.
    deinit:      Option<ffi::Closure1<*mut picom::backend_base, Rc<UnsafeCell<Self>>, ()>>,
    present:     Option<ffi::Closure1<*mut picom::backend_base, Rc<UnsafeCell<Self>>, u8>>,
    root_change: Option<
        ffi::Closure2<*mut picom::backend_base, *mut picom::session, Rc<UnsafeCell<Self>>, ()>,
    >,

    cursor_shader: GLuint,

    pw_rx:         Receiver<MessagesFromPipewire>,
    pw_tx:         Option<PipewireSender>,
    saved_fn_ptrs: SavedFnPtrs,
    buffers:       SlotMap<DefaultKey, CaptureReceiver>,

    cookie: Arc<String>,
}

#[derive(Default)]
struct GlStateGuard {
    texture_2d:      GLint,
    draw_fbo:        GLint,
    read_fbo:        GLint,
    program:         GLint,
    blend_enabled:   GLboolean,
    blend_dst_rgb:   GLint,
    blend_src_rgb:   GLint,
    blend_dst_alpha: GLint,
    blend_src_alpha: GLint,
}

impl GlStateGuard {
    fn save() -> Self {
        let mut ret = Self::default();
        GL.with(|gl| {
            let gl = gl.get().unwrap();
            unsafe {
                gl.GetIntegerv(gl::TEXTURE_BINDING_2D, &mut ret.texture_2d);
                gl.GetIntegerv(gl::DRAW_FRAMEBUFFER_BINDING, &mut ret.draw_fbo);
                gl.GetIntegerv(gl::READ_FRAMEBUFFER_BINDING, &mut ret.read_fbo);
                gl.GetIntegerv(gl::CURRENT_PROGRAM, &mut ret.program);
                gl.GetBooleanv(gl::BLEND, &mut ret.blend_enabled);
                gl.GetIntegerv(gl::BLEND_DST_RGB, &mut ret.blend_dst_rgb);
                gl.GetIntegerv(gl::BLEND_SRC_RGB, &mut ret.blend_src_rgb);
                gl.GetIntegerv(gl::BLEND_DST_ALPHA, &mut ret.blend_dst_alpha);
                gl.GetIntegerv(gl::BLEND_SRC_ALPHA, &mut ret.blend_src_alpha);
            }
        });
        ret
    }
}

impl Drop for GlStateGuard {
    fn drop(&mut self) {
        GL.with(|gl| {
            let gl = gl.get().unwrap();
            unsafe {
                gl.BindTexture(gl::TEXTURE_2D, self.texture_2d as _);
                gl.BindFramebuffer(gl::DRAW_FRAMEBUFFER, self.draw_fbo as _);
                gl.BindFramebuffer(gl::READ_FRAMEBUFFER, self.read_fbo as _);
                gl.UseProgram(self.program as _);
                if self.blend_enabled == gl::TRUE {
                    gl.Enable(gl::BLEND);
                } else {
                    gl.Disable(gl::BLEND);
                }
                gl.BlendFuncSeparate(
                    self.blend_src_rgb as _,
                    self.blend_dst_rgb as _,
                    self.blend_src_alpha as _,
                    self.blend_dst_alpha as _,
                );
            }
        });
    }
}

impl PluginContext {
    fn deinit(&mut self, backend: *mut picom::backend_base) {
        unsafe {
            GL.with(|gl| {
                let gl = gl.get().unwrap();
                gl.DeleteProgram(self.cursor_shader);
            });
            (self.saved_fn_ptrs.deinit)(backend)
        };
    }

    fn blit_cursor_prepare(
        shader: GLuint,
        x: i32,
        y: i32,
        cursor: &Cursor,
        texture_unit: GLint,
    ) -> (GLuint, [GLuint; 2]) {
        GL.with(|gl| {
            let gl = gl.get().unwrap();
            let (x, y) = (x - cursor.hotspot_x as i32, y - cursor.hotspot_y as i32);
            unsafe {
                gl.BindTexture(gl::TEXTURE_2D, cursor.texture);
                gl.UseProgram(shader);
                gl.Uniform1i(0, (texture_unit as u32 - gl::TEXTURE0) as _);

                let mut vao = 0;
                gl.GenVertexArrays(1, &mut vao);
                gl.BindVertexArray(vao);
                let mut vbo = [0; 2];
                gl.GenBuffers(2, vbo.as_mut_ptr());
                gl.BindBuffer(gl::ARRAY_BUFFER, vbo[0]);
                gl.BindBuffer(gl::ELEMENT_ARRAY_BUFFER, vbo[1]);
                let vertices: [f32; 16] = [
                    // pos
                    x as f32,
                    y as f32,
                    (x + cursor.width as i32) as f32,
                    y as f32,
                    (x + cursor.width as i32) as f32,
                    (y + cursor.height as i32) as f32,
                    x as f32,
                    (y + cursor.height as i32) as f32,
                    // uv
                    0.0,
                    0.0,
                    1.0,
                    0.0,
                    1.0,
                    1.0,
                    0.0,
                    1.0,
                ];
                gl.BufferData(
                    gl::ARRAY_BUFFER,
                    (vertices.len() * std::mem::size_of::<f32>()) as _,
                    vertices.as_ptr() as _,
                    gl::STATIC_DRAW,
                );
                let indices: [u32; 6] = [0, 1, 2, 2, 3, 0];
                gl.BufferData(
                    gl::ELEMENT_ARRAY_BUFFER,
                    (indices.len() * std::mem::size_of::<u32>()) as _,
                    indices.as_ptr() as _,
                    gl::STATIC_DRAW,
                );
                gl.EnableVertexAttribArray(0);
                gl.EnableVertexAttribArray(1);
                gl.VertexAttribPointer(
                    0,
                    2,
                    gl::FLOAT,
                    gl::FALSE,
                    (2 * std::mem::size_of::<f32>()) as _,
                    std::ptr::null(),
                );
                gl.VertexAttribPointer(
                    1,
                    2,
                    gl::FLOAT,
                    gl::FALSE,
                    (2 * std::mem::size_of::<f32>()) as _,
                    (8 * std::mem::size_of::<f32>()) as _,
                );
                (vao, vbo)
            }
        })
    }

    fn copy_back_buffer(&mut self) -> anyhow::Result<()> {
        let Self { pw_tx, x11, root, cursor_monitor, pw_rx, root_size, cursor_shader, .. } = self;
        let Some(pw_send) = pw_tx.as_ref() else {
            return Ok(());
        };
        let pw_send = pw_send.clone();

        let pointer = x11.query_pointer(*root)?.reply()?;
        let cursor = cursor_monitor.current_cursor()?;
        let mut pw_tx = pw_send.start_send();
        let mut active_buffers = HashSet::new();
        while let Ok(msg) = pw_rx.try_recv() {
            match msg {
                MessagesFromPipewire::AddBuffer {
                    dma_buf,
                    stream_id,
                    x,
                    y,
                    embed_cursor,
                    reply,
                } => {
                    let image = CaptureReceiver::import(&dma_buf, stream_id, x, y, embed_cursor)
                        .context("import")?;
                    let id = self.buffers.insert(image);
                    reply.send((id, dma_buf)).ok();
                }
                MessagesFromPipewire::ActivateBuffer { id } => {
                    assert!(!active_buffers.contains(&id));
                    active_buffers.insert(id);
                }
                MessagesFromPipewire::RemoveBuffers { ids } => {
                    for id in &ids {
                        self.buffers.remove(*id);
                        active_buffers.remove(id);
                    }
                }
                MessagesFromPipewire::WakeMeUp => {
                    pw_tx.wake();
                }
            }
        }
        if active_buffers.is_empty() {
            return Ok(());
        }
        GL.with(|gl| {
            let gl = gl.get().unwrap();
            unsafe {
                let mut texture_unit = 0;
                gl.GetIntegerv(gl::ACTIVE_TEXTURE, &mut texture_unit);
                let _guard = GlStateGuard::save();
                gl.Enable(gl::BLEND);
                gl.BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
                gl.BindFramebuffer(gl::READ_FRAMEBUFFER, 0);
                let resources = cursor.map(|cursor| {
                    Self::blit_cursor_prepare(
                        *cursor_shader,
                        pointer.root_x as _,
                        pointer.root_y as _,
                        cursor,
                        texture_unit,
                    )
                });
                for i in active_buffers {
                    let b = &self.buffers[i];
                    gl.BindFramebuffer(gl::DRAW_FRAMEBUFFER, b.fbo);
                    tracing::trace!("fbo: {}, size: {}x{}", b.fbo, b.width, b.height);
                    gl.BlitFramebuffer(
                        // src
                        b.x as _,
                        (root_size.height as i32 - b.y) as _,
                        (b.x + b.width as i32) as _,
                        (root_size.height as i32 - b.y - b.height as i32) as _,
                        // dst
                        0,
                        0,
                        b.width as _,
                        b.height as _,
                        gl::COLOR_BUFFER_BIT,
                        gl::NEAREST,
                    );
                    if b.embed_cursor && cursor.is_some() {
                        let offset = [-b.x as f32, -b.y as f32];
                        gl.Uniform2fv(1, 1, offset.as_ptr());
                        gl.DrawElements(gl::TRIANGLES, 6, gl::UNSIGNED_INT, std::ptr::null());
                    }
                    match self.buffers[i].insert_fence() {
                        Ok(fence) => {
                            pw_tx
                                .send(MessagesToPipewire::NewFrame {
                                    id: i,
                                    fence,
                                    stream_id: self.buffers[i].stream_id,
                                })
                                .unwrap()
                        }
                        Err(e) => {
                            tracing::error!("insert_fence failed: {}", e);
                            pw_tx
                                .send(MessagesToPipewire::BufferError {
                                    id:        i,
                                    stream_id: self.buffers[i].stream_id,
                                })
                                .unwrap()
                        }
                    };
                }

                let error = gl.GetError();
                if error != gl::NO_ERROR {
                    tracing::error!("GL errorB: {}", error);
                }
                if let Some((vao, vbo)) = resources {
                    gl.BindBuffer(gl::ARRAY_BUFFER, 0);
                    gl.BindBuffer(gl::ELEMENT_ARRAY_BUFFER, 0);
                    gl.BindVertexArray(0);
                    gl.DeleteVertexArrays(1, &vao);
                    gl.DeleteBuffers(2, vbo.as_ptr());
                }
            }
        });
        Ok(())
    }

    fn present(&mut self, backend: *mut picom::backend_base) -> bool {
        match self.copy_back_buffer() {
            Ok(()) => (),
            Err(e) => {
                tracing::debug!("copy_back_buffer failed: {:?}, bail", e);
                self.pw_tx.take();
            }
        }
        if let Some(present) = self.saved_fn_ptrs.present {
            if !unsafe { present(backend) } {
                return false;
            }
        }
        true
    }

    fn root_change(
        &mut self,
        backend: *mut picom::backend_base,
        session: *mut picom::session,
    ) -> anyhow::Result<()> {
        if let Some(root_change) = self.saved_fn_ptrs.root_change {
            unsafe { root_change(backend, session) }
        }
        let root = self.x11.setup().roots.first().unwrap().root;
        self.root_size = self.x11.get_geometry(root)?.reply()?.into();
        Ok(())
    }
}

fn deinit_trampoline(
    backend: &mut *mut picom::backend_base,
    userdata: *mut Rc<UnsafeCell<PluginContext>>,
) {
    tracing::debug!("userdata refcount: {}", Rc::strong_count(unsafe { &*userdata }));
    let deinit = {
        // This is extremely unsafe. Dropping `userdata.deinit` transitively drops
        // `userdata`, which we still have a mut reference to (!). So we must
        // keep it alive, until we have gotten rid of the mut reference to
        // `userdata`.
        let userdata = unsafe { &mut *(*userdata).get() };
        userdata.deinit(*backend);
        userdata.present.take();
        userdata.root_change.take();
        userdata.deinit.take()
    };

    // Here we don't have mut reference to `userdata` anymore, only a raw ptr. So
    // it's safe to drop `deinit`.
    drop(deinit);
}

fn present_trampoline(
    backend: &mut *mut picom::backend_base,
    userdata: *mut Rc<UnsafeCell<PluginContext>>,
) -> u8 {
    unsafe { &mut *(*userdata).get() }.present(*backend) as _
}

fn root_change_trampoline(
    backend: &mut *mut picom::backend_base,
    session: &mut *mut picom::session,
    userdata: *mut Rc<UnsafeCell<PluginContext>>,
) {
    match unsafe { &mut *(*userdata).get() }.root_change(*backend, *session) {
        Ok(()) => {}
        Err(e) => {
            tracing::debug!("root_change failed: {}", e);
        }
    }
}

fn egl_get_dma_buf_formats(egl: &egl::sys::Egl, dpy: egl::EGLDisplay) -> Vec<egl::EGLint> {
    let mut num: i32 = 0;
    let mut formats = Vec::new();
    unsafe {
        egl.QueryDmaBufFormatsEXT(dpy, 0, std::ptr::null_mut(), &mut num);
        formats.resize(num as usize, 0);
        egl.QueryDmaBufFormatsEXT(dpy, num, formats.as_mut_ptr(), &mut num);
    }
    tracing::debug!("num: {}", num);
    formats
}

fn egl_get_dma_buf_modifiers(
    egl: &egl::sys::Egl,
    dpy: egl::EGLDisplay,
    format: drm_fourcc::DrmFourcc,
) -> Vec<(egl::sys::types::EGLuint64KHR, egl::sys::types::EGLBoolean)> {
    let mut num: i32 = 0;
    let mut modifiers = Vec::new();
    let mut external_only = Vec::new();
    unsafe {
        egl.QueryDmaBufModifiersEXT(
            dpy,
            format as u32 as i32,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut num,
        );
        modifiers.resize(num as usize, 0);
        external_only.resize(num as usize, 0);
        egl.QueryDmaBufModifiersEXT(
            dpy,
            format as u32 as i32,
            num,
            modifiers.as_mut_ptr(),
            external_only.as_mut_ptr(),
            &mut num,
        );
    }
    modifiers.into_iter().zip(external_only).collect()
}

const REQUIRED_EGL_CLIENT_EXTENSIONS: &[&str] = &["EGL_EXT_device_base", "EGL_EXT_device_query"];
const REQUIRED_EGL_DEVICE_EXTENSIONS: &[&str] = &["EGL_EXT_device_drm_render_node"];
const REQUIRED_EGL_EXTENSIONS: &[&str] = &["EGL_EXT_image_dma_buf_import_modifiers"];

unsafe fn egl_check_extensions(egl: &egl::sys::Egl, dpy: egl::EGLDisplay) -> anyhow::Result<()> {
    let egl_extensions: HashSet<&str> =
        std::ffi::CStr::from_ptr(egl.QueryString(egl::sys::NO_DISPLAY, egl::sys::EXTENSIONS as _))
            .to_str()?
            .split(' ')
            .collect();
    for required in REQUIRED_EGL_CLIENT_EXTENSIONS {
        if !egl_extensions.contains(required) {
            return Err(anyhow::anyhow!("Required EGL client extension {} not found", required));
        }
    }
    let egl_extensions: HashSet<&str> =
        std::ffi::CStr::from_ptr(egl.QueryString(dpy, egl::sys::EXTENSIONS as _))
            .to_str()?
            .split(' ')
            .collect();
    for required in REQUIRED_EGL_EXTENSIONS {
        if !egl_extensions.contains(required) {
            return Err(anyhow::anyhow!("Required EGL extension {} not found", required));
        }
    }
    Ok(())
}

struct DrmRenderNode(std::fs::File);
impl AsFd for DrmRenderNode {
    fn as_fd(&self) -> std::os::unix::io::BorrowedFd { self.0.as_fd() }
}
impl drm::Device for DrmRenderNode {
}

struct CaptureReceiver {
    texture: GLuint,
    image: egl::sys::types::EGLImage,
    fbo: GLuint,
    stream_id: DefaultKey,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    embed_cursor: bool,
}

// We have 3 threads:
// 1. picom thread, that's the compositor's main thread, which calls into hooks
//    registered by our plugin.
// 3. pipewire thread. This is the thread that runs the pipewire mainloop and
//    handles pipewire communication.

#[derive(Debug)]
enum MessagesToPipewire {
    /// Sent when picom presents a new frame, after we have sent commands to
    /// copy this frame into our buffer. After sending this, buffer `id`
    /// will stop being active.
    NewFrame {
        id:        DefaultKey,
        fence:     drm::control::syncobj::Handle,
        stream_id: DefaultKey,
    },
    /// Error occurred for the given buffer. The pipewire thread should drop
    /// this buffer, and the stream it's associated with.
    BufferError { id: DefaultKey, stream_id: DefaultKey },
    CreateStream {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        embed_cursor: bool,
        reply: oneshot::Sender<anyhow::Result<u32>>,
    },
}

enum MessagesFromPipewire {
    /// A new buffer is created on the pipewire's side, the main thread
    /// should import it, and send back an id in a `BufferImported` response.
    AddBuffer {
        dma_buf: gbm::BufferObject<()>,
        stream_id: DefaultKey,
        x: i32,
        y: i32,
        embed_cursor: bool,
        reply: oneshot::Sender<(DefaultKey, gbm::BufferObject<()>)>,
    },
    /// Set buffer active. There can be multiple active buffers at the same
    /// time.
    ActivateBuffer {
        id: DefaultKey,
    },
    /// Clear all imported buffers, this happens when the pipewire thread is
    /// shutting down.
    RemoveBuffers {
        ids: SmallVec<[DefaultKey; 2]>,
    },
    WakeMeUp,
}

thread_local! {
    static EGL: OnceCell<egl::sys::Egl> = const{ OnceCell::new() };
    static GL: OnceCell<gl::Gl> = const { OnceCell::new() };
}

impl Drop for CaptureReceiver {
    fn drop(&mut self) {
        GL.with(|gl| {
            let gl = gl.get().unwrap();
            unsafe {
                gl.DeleteTextures(1, &self.texture);
                gl.DeleteFramebuffers(1, &self.fbo);
            }
        });
        EGL.with(|egl| {
            let egl = egl.get().unwrap();
            unsafe {
                let dpy = egl.GetCurrentDisplay();
                egl.DestroyImage(dpy, self.image);
            }
        });
    }
}

struct EglDmaBufParamterIds {
    fd_ext:          egl::EGLenum,
    offset_ext:      egl::EGLenum,
    pitch_ext:       egl::EGLenum,
    modifier_lo_ext: egl::EGLenum,
    modifier_hi_ext: egl::EGLenum,
}

const EGL_DMA_BUF_PARAMETER_IDS: [EglDmaBufParamterIds; 4] = [
    EglDmaBufParamterIds {
        fd_ext:          egl::sys::DMA_BUF_PLANE0_FD_EXT,
        offset_ext:      egl::sys::DMA_BUF_PLANE0_OFFSET_EXT,
        pitch_ext:       egl::sys::DMA_BUF_PLANE0_PITCH_EXT,
        modifier_lo_ext: egl::sys::DMA_BUF_PLANE0_MODIFIER_LO_EXT,
        modifier_hi_ext: egl::sys::DMA_BUF_PLANE0_MODIFIER_HI_EXT,
    },
    EglDmaBufParamterIds {
        fd_ext:          egl::sys::DMA_BUF_PLANE1_FD_EXT,
        offset_ext:      egl::sys::DMA_BUF_PLANE1_OFFSET_EXT,
        pitch_ext:       egl::sys::DMA_BUF_PLANE1_PITCH_EXT,
        modifier_lo_ext: egl::sys::DMA_BUF_PLANE1_MODIFIER_LO_EXT,
        modifier_hi_ext: egl::sys::DMA_BUF_PLANE1_MODIFIER_HI_EXT,
    },
    EglDmaBufParamterIds {
        fd_ext:          egl::sys::DMA_BUF_PLANE2_FD_EXT,
        offset_ext:      egl::sys::DMA_BUF_PLANE2_OFFSET_EXT,
        pitch_ext:       egl::sys::DMA_BUF_PLANE2_PITCH_EXT,
        modifier_lo_ext: egl::sys::DMA_BUF_PLANE2_MODIFIER_LO_EXT,
        modifier_hi_ext: egl::sys::DMA_BUF_PLANE2_MODIFIER_HI_EXT,
    },
    EglDmaBufParamterIds {
        fd_ext:          egl::sys::DMA_BUF_PLANE3_FD_EXT,
        offset_ext:      egl::sys::DMA_BUF_PLANE3_OFFSET_EXT,
        pitch_ext:       egl::sys::DMA_BUF_PLANE3_PITCH_EXT,
        modifier_lo_ext: egl::sys::DMA_BUF_PLANE3_MODIFIER_LO_EXT,
        modifier_hi_ext: egl::sys::DMA_BUF_PLANE3_MODIFIER_HI_EXT,
    },
];

impl CaptureReceiver {
    fn insert_fence(&mut self) -> anyhow::Result<drm::control::syncobj::Handle> {
        EGL.with(|egl| {
            let egl = egl.get().unwrap();
            unsafe {
                let dpy = egl.GetCurrentDisplay();
                let fence =
                    egl.CreateSyncKHR(dpy, egl::sys::SYNC_NATIVE_FENCE_ANDROID, std::ptr::null());
                if fence == egl::sys::NO_SYNC {
                    return Err(anyhow::anyhow!("CreateSyncKHR failed"));
                }
                GL.with(|gl| {
                    let gl = gl.get().unwrap();
                    gl.Flush();
                });
                let fence_fd = egl.DupNativeFenceFDANDROID(dpy, fence);
                if fence_fd == egl::sys::NO_NATIVE_FENCE_FD_ANDROID {
                    return Err(anyhow::anyhow!("GetSyncAttrib failed"));
                };
                let fence_fd = std::num::NonZeroU32::new(fence_fd as u32)
                    .ok_or_else(|| anyhow::anyhow!("DupNativeFenceFD failed: fence_fd is zero"))?;
                egl.DestroySyncKHR(dpy, fence);
                Ok(fence_fd.into())
            }
        })
    }

    fn import(
        dma_buf: &gbm::BufferObject<()>,
        stream_id: DefaultKey,
        x: i32,
        y: i32,
        embed_cursor: bool,
    ) -> anyhow::Result<Self> {
        let modifier = dma_buf.modifier()?;
        let raw_modifier: u64 = modifier.into();
        let fds = (0..dma_buf.plane_count()?)
            .map(|i| dma_buf.fd_for_plane(i as i32))
            .collect::<Result<Vec<_>, _>>()?;
        let width = dma_buf.width()?;
        let height = dma_buf.height()?;
        tracing::debug!("Importing: {}x{}", width, height);
        let image = EGL.with(|egl| {
            let egl = egl.get().unwrap();
            let mut attribs = vec![
                egl::sys::LINUX_DRM_FOURCC_EXT as isize,
                dma_buf.format()? as _,
                egl::sys::WIDTH as _,
                width as _,
                egl::sys::HEIGHT as _,
                height as _,
            ];
            for plane_id in 0..(dma_buf.plane_count()? as i32) {
                let param_ids = &EGL_DMA_BUF_PARAMETER_IDS[plane_id as usize];
                attribs.extend_from_slice(&[
                    param_ids.fd_ext as isize,
                    fds[plane_id as usize].as_raw_fd() as _,
                    param_ids.offset_ext as isize,
                    dma_buf.offset(plane_id)? as _,
                    param_ids.pitch_ext as isize,
                    dma_buf.stride_for_plane(plane_id)? as _,
                    param_ids.modifier_lo_ext as isize,
                    (raw_modifier & 0xffffffff) as _,
                    param_ids.modifier_hi_ext as isize,
                    (raw_modifier >> 32) as _,
                ]);
            }
            attribs.extend(&[egl::sys::NONE as isize]);
            Ok::<_, anyhow::Error>(unsafe {
                egl.CreateImage(
                    egl.GetCurrentDisplay(),
                    egl::sys::NO_CONTEXT,
                    egl::sys::LINUX_DMA_BUF_EXT,
                    std::ptr::null(),
                    attribs.as_ptr(),
                )
            })
        })?;
        if image == egl::sys::NO_IMAGE {
            return Err(anyhow::anyhow!(
                "CreateImage failed {:x}",
                EGL.with(|egl| unsafe { egl.get().unwrap().GetError() })
            ));
        }
        let (fbo, texture) = GL.with(|gl| {
            let gl = gl.get().unwrap();
            let mut texture = 0;
            let mut fbo = 0;
            unsafe {
                let mut old_texture = 0;
                let mut old_draw_fbo = 0;
                gl.GetIntegerv(gl::TEXTURE_BINDING_2D, &mut old_texture);
                gl.GetIntegerv(gl::DRAW_FRAMEBUFFER_BINDING, &mut old_draw_fbo);
                gl.GenTextures(1, &mut texture);
                gl.BindTexture(gl::TEXTURE_2D, texture);
                gl.EGLImageTargetTexStorageEXT(gl::TEXTURE_2D, image, std::ptr::null());
                gl.BindTexture(gl::TEXTURE_2D, old_texture as _);
                gl.GenFramebuffers(1, &mut fbo);
                gl.BindFramebuffer(gl::DRAW_FRAMEBUFFER, fbo);
                gl.FramebufferTexture2D(
                    gl::DRAW_FRAMEBUFFER,
                    gl::COLOR_ATTACHMENT0,
                    gl::TEXTURE_2D,
                    texture,
                    0,
                );
                let status = gl.CheckFramebufferStatus(gl::DRAW_FRAMEBUFFER);
                if status != gl::FRAMEBUFFER_COMPLETE {
                    return Err(anyhow::anyhow!("Framebuffer incomplete: {:x}", status));
                }
                gl.BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
                gl.ClearColor(1.0, 0.0, 0.0, 1.0);
                gl.Clear(gl::COLOR_BUFFER_BIT);
                tracing::debug!("GL error: {:x}", gl.GetError());

                gl.BindFramebuffer(gl::DRAW_FRAMEBUFFER, old_draw_fbo as _);
            }
            Ok((fbo, texture))
        })?;
        Ok(Self { fbo, texture, image, stream_id, width, height, x, y, embed_cursor })
    }
}

bitfield::bitfield! {
    struct AmdModifier(u64);
    impl Debug;
    pub tile_version, _: 7, 0;
    pub tile, _: 12, 8;
    pub dcc, _: 13;
    pub dcc_retile, _: 14;
    pub dcc_pipe_align, _: 15;
    pub dcc_independent_64b, _: 16;
    pub dcc_independent_128b, _: 17;
    pub dcc_max_compressed_block, _: 19, 18;
    pub dcc_constant_encode, _: 20;
    pub pipe_xor_bits, _: 23, 21;
    pub bank_xor_bits, _: 26, 24;
    pub packers, _: 29, 27;
    pub rb, _: 32, 30;
    pub pipe, _: 35, 33;
    pub reserved, _: 55, 36;
    pub vendor, _: 63, 56;
}

bitfield::bitfield! {
    struct Modifier(u64);
    pub reserved, _: 55, 0;
    pub vendor, _: 63, 56;
}

const MODIFIER_VENDOR_AMD: u64 = 0x2;
#[allow(dead_code)]
const AMD_FMT_MOD_TILE_VER_GFX9: u64 = 0x1;
const AMD_FMT_MOD_TILE_VER_GFX10: u64 = 0x2;
#[allow(dead_code)]
const AMD_FMT_MOD_TILE_VER_GFX10_RBPLUS: u64 = 0x3;
#[allow(dead_code)]
const AMD_FMT_MOD_TILE_VER_GFX11: u64 = 0x4;

fn extra_modifier_check(modifier: u64, width: u16, height: u16) -> bool {
    let modifier = Modifier(modifier);
    if modifier.vendor() != MODIFIER_VENDOR_AMD {
        return true;
    }

    let amd_modifier = AmdModifier(modifier.0);
    if width <= 2560 && height <= 2560 {
        // Allocating 2560x2560 buffers are always possible
        return true;
    }
    if amd_modifier.tile_version() < AMD_FMT_MOD_TILE_VER_GFX10 {
        // GPUs earlier than GFX10 doesn't have size restriction
        return true;
    }

    !amd_modifier.dcc() || amd_modifier.dcc_independent_64b()
}

#[repr(C)]
struct PicomXConnection {
    xcb:     *mut c_void,
    display: *mut c_void,
    screen:  libc::c_int,
}
const BASE64_CHARSET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
unsafe fn compile_shader(gl: &gl::Gl, source: &str, t: GLuint) -> anyhow::Result<GLuint> {
    let shader = gl.CreateShader(t);
    gl.ShaderSource(
        shader,
        1,
        [source.as_ptr() as *const _].as_ptr(),
        [source.len() as _].as_ptr(),
    );
    gl.CompileShader(shader);
    let mut success = 0;
    gl.GetShaderiv(shader, gl::COMPILE_STATUS, &mut success);
    if success != gl::TRUE as i32 {
        let mut len = 0;
        gl.GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut len);
        let mut buf = vec![0; len as usize];
        gl.GetShaderInfoLog(shader, len, std::ptr::null_mut(), buf.as_mut_ptr() as _);
        tracing::error!("Shader compilation failed: {}", std::str::from_utf8(&buf).unwrap());
        gl.DeleteShader(shader);
        return Err(anyhow::anyhow!("Shader compilation failed"));
    }
    Ok(shader)
}
unsafe fn backend_plugin_init_inner(backend: &mut picom::backend_base) -> anyhow::Result<()> {
    tracing::debug!("backend_plugin_init called {backend:p}");
    {
        let mut major = 0;
        let mut minor = 0;
        (backend.ops.version.unwrap())(backend, &mut major, &mut minor);
        if major != 0 || minor < 1 {
            return Err(anyhow::anyhow!("Unsupported picom egl backend version"));
        }
    }

    let egl = egl::sys::Egl::load_with(|name| {
        let name = std::ffi::CString::new(name).unwrap();
        eglGetProcAddress(name.as_ptr())
    });
    let dpy = egl.GetCurrentDisplay();

    egl_check_extensions(&egl, dpy)?;

    let mut device: egl::sys::types::EGLAttrib = 0;
    if egl.QueryDisplayAttribEXT(dpy, egl::sys::DEVICE_EXT as i32, &mut device) == 0 {
        return Err(anyhow::anyhow!("QueryDisplayAttribEXT failed"));
    }

    tracing::debug!("device: {:#x}", device);
    let egl_extensions: HashSet<&str> = std::ffi::CStr::from_ptr(
        egl.QueryDeviceStringEXT(device as u64 as _, egl::sys::EXTENSIONS as _),
    )
    .to_str()?
    .split(' ')
    .collect();
    for required in REQUIRED_EGL_DEVICE_EXTENSIONS {
        if !egl_extensions.contains(required) {
            return Err(anyhow::anyhow!("Required EGL device extension {} not found", required));
        }
    }
    let render_node =
        egl.QueryDeviceStringEXT(device as u64 as _, egl::sys::DRM_RENDER_NODE_FILE_EXT as i32);
    if render_node.is_null() {
        return Err(anyhow::anyhow!("QueryDeviceStringEXT failed"));
    }
    let render_node = std::ffi::CStr::from_ptr(render_node).to_str().unwrap();
    tracing::debug!("render_node: {}", render_node);

    let render_node = std::fs::File::open(render_node).map(DrmRenderNode)?;
    let gbm = gbm::Device::new(render_node)?;

    let formats: HashSet<egl::EGLint> = egl_get_dma_buf_formats(&egl, dpy).into_iter().collect();
    for format in formats.iter() {
        tracing::debug!(
            "format: {}{}{}{}",
            (format & 0xff) as u8 as char,
            ((format >> 8) & 0xff) as u8 as char,
            ((format >> 16) & 0xff) as u8 as char,
            ((format >> 24) & 0xff) as u8 as char
        );
    }

    let c = unsafe { &mut *(backend.c as *mut PicomXConnection) };
    let x11 = x11rb::xcb_ffi::XCBConnection::from_raw_xcb_connection(c.xcb, false)?;
    let root = x11.setup().roots.first().ok_or_else(|| anyhow::anyhow!("No root found"))?;
    let r = x11.get_geometry(root.root)?.reply()?;
    tracing::info!("Root size: {}x{}", r.width, r.height);

    let compositor_selection = format!("_NET_WM_CM_S{}", c.screen);
    let atom = x11.intern_atom(false, compositor_selection.as_bytes())?.reply()?.atom;
    let selection_owner = x11.get_selection_owner(atom)?.reply()?.owner;

    tracing::info!("Selection owner: {selection_owner:#x}");

    let formats_modifiers: Vec<_> = formats
        .into_iter()
        .filter_map(|format| (format as u32).try_into().ok())
        .filter_map(|format| {
            let spa_format = pipewire::fourcc_to_spa_format(format)?;
            let modifiers = egl_get_dma_buf_modifiers(&egl, dpy, format);
            for modifier in &modifiers {
                tracing::debug!(
                    "format {format} modifier: {:x}, external_only: {}",
                    modifier.0,
                    modifier.1
                );
            }
            let modifiers: Vec<DrmModifier> = modifiers
                .into_iter()
                .filter(|(modifier, external_only)| {
                    *external_only == 0 && extra_modifier_check(*modifier, r.width, r.height)
                })
                .map(|(modifiers, _)| modifiers.into())
                .collect();
            if !modifiers.is_empty() {
                Some((spa_format, modifiers))
            } else {
                None
            }
        })
        .collect();
    if formats_modifiers.is_empty() {
        return Err(anyhow::anyhow!("No suitable modifier/formats found"));
    };

    let backend = &mut *backend;
    let (our_pw_waker, pw_waker) = ::pipewire::channel::channel();
    let (our_pw_tx, pw_rx) = std::sync::mpsc::channel();
    let (pw_tx, our_pw_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        match pipewire::pipewire_main(gbm, pw_waker, pw_rx, pw_tx, formats_modifiers) {
            Ok(()) => {}
            Err(e) => {
                tracing::debug!("pipewire_main failed: {e:?}");
            }
        }
    });

    let cursor_monitor = cursor::CursorMonitor::new(egl.GetCurrentContext(), dpy)?;
    // Compile shaders
    let gl = gl::Gl::load_with(|name| {
        let name = std::ffi::CString::new(name).unwrap();
        eglGetProcAddress(name.as_ptr())
    });
    let vs = compile_shader(&gl, COPY_VS.to_str().unwrap(), gl::VERTEX_SHADER)?;
    let fs = compile_shader(&gl, COPY_FS.to_str().unwrap(), gl::FRAGMENT_SHADER)?;
    let program = gl.CreateProgram();
    gl.AttachShader(program, vs);
    gl.AttachShader(program, fs);
    gl.LinkProgram(program);
    gl.DeleteShader(vs);
    gl.DeleteShader(fs);
    let mut success = 0;
    gl.GetProgramiv(program, gl::LINK_STATUS, &mut success);
    if success != gl::TRUE as i32 {
        let mut len = 0;
        gl.GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut len);
        let mut buf = vec![0; len as usize];
        gl.GetProgramInfoLog(program, len, std::ptr::null_mut(), buf.as_mut_ptr() as _);
        tracing::error!("Program link failed: {}", std::str::from_utf8(&buf).unwrap());
        gl.DeleteProgram(program);
        return Err(anyhow::anyhow!("Program link failed"));
    }
    let mut viewport_size = [0; 4];
    gl.GetIntegerv(gl::VIEWPORT, viewport_size.as_mut_ptr());
    assert_eq!(viewport_size[0], 0);
    assert_eq!(viewport_size[1], 0);
    let [_, _, width, height] = viewport_size;
    let scale = [2.0 / width as f32, 2.0 / height as f32];

    let mut old_program = 0;
    gl.GetIntegerv(gl::CURRENT_PROGRAM, &mut old_program);
    gl.UseProgram(program);
    gl.Uniform2fv(2, 1, scale.as_ptr());
    gl.UseProgram(old_program as _);

    let context = Rc::new(UnsafeCell::new(PluginContext {
        deinit: None,
        present: None,
        root_change: None,
        cursor_monitor,
        cursor_shader: program,
        saved_fn_ptrs: SavedFnPtrs {
            deinit:      backend.ops.deinit.unwrap_unchecked(),
            present:     backend.ops.present,
            root_change: backend.ops.root_change,
        },
        root: root.root,
        x11,
        root_size: r.into(),
        buffers: Default::default(),
        pw_rx: our_pw_rx,
        pw_tx: Some(PipewireSender::new(our_pw_waker, our_pw_tx)),
        cookie: Arc::new(random_string::generate(88, BASE64_CHARSET)),
    }));
    {
        let context_mut = unsafe { &mut *context.get() };
        backend.ops.deinit = Some(std::mem::transmute::<
            *mut libc::c_void,
            unsafe extern "C" fn(*mut picom::backend_base),
        >(
            context_mut
                .deinit
                .insert(ffi::make_ffi_closure1(deinit_trampoline, context.clone())?)
                .code_ptr(),
        ));
        backend.ops.present = Some(std::mem::transmute::<
            *mut libc::c_void,
            unsafe extern "C" fn(*mut picom::backend_base) -> bool,
        >(
            context_mut
                .present
                .insert(ffi::make_ffi_closure1(present_trampoline, context.clone())?)
                .code_ptr(),
        ));
        backend.ops.root_change = Some(std::mem::transmute::<
            *mut libc::c_void,
            unsafe extern "C" fn(*mut picom::backend_base, *mut picom::session),
        >(
            context_mut
                .root_change
                .insert(ffi::make_ffi_closure2(root_change_trampoline, context.clone())?)
                .code_ptr(),
        ));

        let egl_screencast_atom =
            context_mut.x11.intern_atom(false, b"EGL_SCREENCAST_COOKIE")?.reply()?.atom;

        let utf_string_atom = context_mut.x11.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;

        context_mut
            .x11
            .change_property(
                PropMode::REPLACE,
                selection_owner,
                egl_screencast_atom,
                utf_string_atom,
                8,
                context_mut.cookie.len() as _,
                context_mut.cookie.as_bytes(),
            )?
            .check()?;

        std::thread::spawn({
            let cookie = context_mut.cookie.clone();
            let pw_tx = context_mut.pw_tx.clone().unwrap();
            move || server::start_server(cookie, pw_tx, selection_owner)
        });
    }
    EGL.with(|egl_| assert!(egl_.set(egl).is_ok()));
    GL.with(|gl_| assert!(gl_.set(gl).is_ok()));
    Ok(())
}

/// # Safety
pub unsafe extern "C" fn backend_plugin_init(backend: *mut picom::backend_base, _: *mut c_void) {
    if let Err(e) = backend_plugin_init_inner(&mut *backend) {
        tracing::debug!("backend_plugin_init failed: {e:?}");
    }
}

#[cfg(not(test))]
#[ctor::ctor]
unsafe fn init() {
    tracing_subscriber::fmt::init();
    tracing::debug!("init called");

    let api = &*picom::picom_api_get_interfaces(0, 1, c"egl-screencast".as_ptr());
    (api.add_backend_plugin.unwrap())(
        c"egl".as_ptr(),
        1,
        0,
        Some(backend_plugin_init),
        std::ptr::null_mut(),
    );
}
