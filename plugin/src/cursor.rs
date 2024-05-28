//! Cursor monitor
//!
//! Monitor cursor changes and import them into GL textures

use std::collections::{HashMap, HashSet};

use gl_bindings::{egl, gl};
use x11rb::{connection::Connection as _, protocol::xfixes::ConnectionExt as _};

#[derive(Debug)]
pub(crate) struct Cursor {
    pub(crate) hotspot_x: u32,
    pub(crate) hotspot_y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) texture: gl::types::GLuint,
}

#[derive(Debug)]
pub(crate) enum CursorMessage {
    NewCursor { serial: u32, cursor: Cursor },
    ChangeCursor { serial: u32 },
}

struct CursorMonitorImpl {
    egl: egl::sys::Egl,
    gl: gl::Gl,

    textures: Vec<gl::types::GLuint>,
    cursor_serials: HashSet<u32>,

    x11: x11rb::rust_connection::RustConnection,
    egl_ctx: egl::sys::types::EGLContext,
    egl_display: egl::sys::types::EGLDisplay,
    tx: std::sync::mpsc::Sender<CursorMessage>,
}
impl CursorMonitorImpl {
    fn run_inner(&mut self, screen: usize) -> anyhow::Result<()> {
        let root = self.x11.setup().roots[screen].root;
        let (major, minor) = x11rb::protocol::xfixes::X11_XML_VERSION;
        self.x11.xfixes_query_version(major, minor)?.reply()?;
        self.x11
            .xfixes_select_cursor_input(
                root,
                x11rb::protocol::xfixes::CursorNotifyMask::DISPLAY_CURSOR,
            )?
            .check()?;
        loop {
            use x11rb::protocol::Event::*;
            let event = self.x11.wait_for_event()?;
            #[allow(clippy::single_match)]
            match event {
                XfixesCursorNotify(notify) => {
                    if self.cursor_serials.contains(&notify.cursor_serial) {
                        if self
                            .tx
                            .send(CursorMessage::ChangeCursor {
                                serial: notify.cursor_serial,
                            })
                            .is_err()
                        {
                            break;
                        }
                    } else {
                        let cursor = self.x11.xfixes_get_cursor_image()?.reply()?;
                        let mut texture = 0;
                        unsafe {
                            self.gl.GenTextures(1, &mut texture);
                            self.gl.BindTexture(gl::TEXTURE_2D, texture);
                            self.gl.TexImage2D(
                                gl::TEXTURE_2D,
                                0,
                                gl::RGBA as _,
                                cursor.width as _,
                                cursor.height as _,
                                0,
                                gl::BGRA,
                                gl::UNSIGNED_BYTE,
                                cursor.cursor_image.as_ptr() as _,
                            );
                            self.gl.TexParameteri(
                                gl::TEXTURE_2D,
                                gl::TEXTURE_MIN_FILTER,
                                gl::NEAREST as _,
                            );
                            self.gl.TexParameteri(
                                gl::TEXTURE_2D,
                                gl::TEXTURE_MAG_FILTER,
                                gl::NEAREST as _,
                            );
                            self.gl.TexParameteri(
                                gl::TEXTURE_2D,
                                gl::TEXTURE_WRAP_S,
                                gl::CLAMP_TO_EDGE as _,
                            );
                            self.gl.TexParameteri(
                                gl::TEXTURE_2D,
                                gl::TEXTURE_WRAP_T,
                                gl::CLAMP_TO_EDGE as _,
                            );
                            self.gl.Flush();
                            self.gl.Finish();
                        }
                        self.textures.push(texture);
                        self.cursor_serials.insert(cursor.cursor_serial);
                        let err = unsafe { self.gl.GetError() };
                        if err != gl::NO_ERROR {
                            tracing::error!("GL error: {}", err);
                        }

                        if self
                            .tx
                            .send(CursorMessage::NewCursor {
                                serial: cursor.cursor_serial,
                                cursor: Cursor {
                                    hotspot_x: cursor.xhot as _,
                                    hotspot_y: cursor.yhot as _,
                                    width: cursor.width as _,
                                    height: cursor.height as _,
                                    texture,
                                },
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn run(mut self, screen: usize) {
        if let Err(e) = self.run_inner(screen) {
            tracing::error!("Error in cursor monitor: {:?}", e);
        }
        for texture in self.textures {
            unsafe {
                self.gl.DeleteTextures(1, &texture);
            }
        }
        unsafe {
            self.egl.MakeCurrent(
                self.egl_display,
                egl::sys::NO_SURFACE,
                egl::sys::NO_SURFACE,
                egl::sys::NO_CONTEXT,
            );
            self.egl.DestroyContext(self.egl_display, self.egl_ctx);
        };
    }
}
unsafe impl Send for CursorMonitorImpl {}

pub(crate) struct CursorMonitor {
    rx: std::sync::mpsc::Receiver<CursorMessage>,
    cursors: HashMap<u32, Cursor>,
    current_cursor: Option<u32>,
}

impl CursorMonitor {
    fn next(&self) -> anyhow::Result<Option<CursorMessage>> {
        Ok(self.rx.try_recv().map(Some).or_else(|e| {
            if e == std::sync::mpsc::TryRecvError::Empty {
                Ok(None)
            } else {
                Err(e)
            }
        })?)
    }
    pub(crate) fn current_cursor(&mut self) -> anyhow::Result<Option<&Cursor>> {
        while let Some(msg) = self.next()? {
            tracing::debug!("Cursor message: {:?}", msg);
            match msg {
                CursorMessage::NewCursor { serial, cursor } => {
                    self.cursors.insert(serial, cursor);
                    self.current_cursor = Some(serial);
                }
                CursorMessage::ChangeCursor { serial } => {
                    self.current_cursor = Some(serial);
                }
            }
        }
        Ok(self.current_cursor.and_then(|c| self.cursors.get(&c)))
    }
    pub(crate) fn new(
        ctx: egl::sys::types::EGLContext,
        dpy: egl::sys::types::EGLDisplay,
    ) -> anyhow::Result<Self> {
        let egl = egl::sys::Egl::load_with(|sym| {
            std::ffi::CString::new(sym.as_bytes())
                .map(|s| unsafe { crate::eglGetProcAddress(s.as_ptr()) })
                .unwrap()
        });
        let gl = gl::Gl::load_with(|sym| {
            std::ffi::CString::new(sym.as_bytes())
                .map(|s| unsafe { crate::eglGetProcAddress(s.as_ptr()) })
                .unwrap()
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let (x11, screen) = x11rb::rust_connection::RustConnection::connect(None)?;
        let ctx = unsafe { egl.CreateContext(dpy, std::ptr::null(), ctx, std::ptr::null()) };
        if ctx == egl::sys::NO_CONTEXT {
            return Err(anyhow::anyhow!("Failed to create context"));
        }
        let monitor = CursorMonitorImpl {
            x11,
            egl,
            gl,
            egl_ctx: ctx,
            egl_display: dpy,
            tx,
            textures: Vec::new(),
            cursor_serials: HashSet::new(),
        };

        std::thread::spawn(move || {
            if unsafe {
                monitor.egl.MakeCurrent(
                    monitor.egl_display,
                    egl::sys::NO_SURFACE,
                    egl::sys::NO_SURFACE,
                    monitor.egl_ctx,
                )
            } == egl::sys::FALSE
            {
                return;
            }
            monitor.run(screen);
            tracing::warn!("Cursor monitor thread exited");
        });

        Ok(Self {
            rx,
            cursors: HashMap::new(),
            current_cursor: None,
        })
    }
}
