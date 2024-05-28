use anyhow::Context;
use async_io::Async;
use futures_util::{
    stream::FuturesOrdered, AsyncRead as _, AsyncWrite, Sink, SinkExt, Stream, StreamExt as _,
};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::{
    os::unix::{ffi::OsStrExt, net::UnixStream},
    pin::Pin,
    sync::atomic::AtomicUsize,
    task::ready,
};
use x11rb_async::{
    connection::Connection,
    protocol::{randr::ConnectionExt, xproto::ConnectionExt as _},
};
use zbus::zvariant;
use zvariant::{DeserializeDict, SerializeDict, Type};

struct Picom {
    conn: Async<UnixStream>,
    buf: Vec<u8>,
    len: Option<u32>,
    pos: usize,
    cookie: String,

    out_buf: Vec<u8>,
    out_pos: usize,
}

impl std::fmt::Debug for Picom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Picom").finish()
    }
}

impl Sink<protocol::ClientMessage> for Picom {
    type Error = anyhow::Error;
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.conn).poll_close(cx).map_err(Into::into)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_ready(cx)?);
        Pin::new(&mut self.conn).poll_flush(cx).map_err(Into::into)
    }
    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        while self.out_pos < self.out_buf.len() {
            let Self {
                conn,
                out_buf,
                out_pos,
                ..
            } = &mut *self;
            let written = ready!(Pin::new(&mut *conn).poll_write(cx, &out_buf[*out_pos..]))?;
            tracing::info!("written: {}", written);
            self.out_pos += written;
        }
        self.out_buf.clear();
        self.out_pos = 0;
        std::task::Poll::Ready(Ok(()))
    }
    fn start_send(
        mut self: Pin<&mut Self>,
        item: protocol::ClientMessage,
    ) -> Result<(), Self::Error> {
        let mut cursor = std::io::Cursor::new(&mut self.out_buf);
        cursor.set_position(4);
        serde_json::to_writer(&mut cursor, &item)?;

        let len = cursor.position() as u32 - 4;
        self.out_buf[..4].copy_from_slice(&len.to_be_bytes()[..]);
        Ok(())
    }
}

impl Stream for Picom {
    type Item = anyhow::Result<protocol::ServerMessage>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            if let Some(len) = self.len {
                let Self { conn, buf, pos, .. } = &mut *self;
                buf.resize(len as usize, 0);
                let nbytes = ready!(Pin::new(&mut *conn).poll_read(cx, &mut buf[*pos..]))?;

                self.pos += nbytes;
                if self.pos < len as usize {
                    continue;
                }

                let ret = serde_json::from_slice(&self.buf)?;
                self.len = None;
                self.pos = 0;
                self.buf.clear();
                break std::task::Poll::Ready(Some(Ok(ret)));
            } else {
                self.buf.resize(4, 0);
                let Self { conn, buf, pos, .. } = &mut *self;
                let nbytes = ready!(Pin::new(&mut *conn).poll_read(cx, &mut buf[*pos..]))?;
                self.pos += nbytes;
                if self.pos < 4 {
                    continue;
                }
                self.len = Some(u32::from_be_bytes(self.buf[..4].try_into()?));
                tracing::info!("incoming len: {:?}", self.len);
                self.pos = 0;
                self.buf.clear();
            }
        }
    }
}
async fn get_atom(
    x11: &x11rb_async::rust_connection::RustConnection,
    name: &[u8],
) -> anyhow::Result<u32> {
    Ok(x11.intern_atom(false, name).await?.reply().await?.atom)
}
impl Picom {
    async fn new(
        x11: &x11rb_async::rust_connection::RustConnection,
        screen: usize,
    ) -> anyhow::Result<Self> {
        let compositor_selection = format!("_NET_WM_CM_S{}", screen);
        let futs: FuturesOrdered<_> = [
            get_atom(x11, compositor_selection.as_bytes()),
            get_atom(x11, b"EGL_SCREENCAST_COOKIE"),
            get_atom(x11, b"EGL_SCREENCAST_SOCKET"),
            get_atom(x11, b"UTF8_STRING"),
        ]
        .into_iter()
        .collect();
        let [compositor_selection, egl_screencast_cookie_atom, egl_screencast_socket_atom, utf8_string_atom] =
            <[_; 4]>::try_from(futs.collect::<Vec<_>>().await).unwrap();
        let selection_owner = x11
            .get_selection_owner(compositor_selection?)
            .await?
            .reply()
            .await?
            .owner;
        tracing::info!("selection_owner: {selection_owner:#x}");
        let utf8_string_atom = utf8_string_atom?;
        let (cookie, path) = if selection_owner != x11rb::NONE {
            let egl_screencast_cookie = x11
                .get_property(
                    false,
                    selection_owner,
                    egl_screencast_cookie_atom?,
                    utf8_string_atom,
                    0,
                    128,
                )
                .await?
                .reply()
                .await?;
            if egl_screencast_cookie.type_ == x11rb::NONE {
                return Err(anyhow::anyhow!("No cookie found"));
            }
            let egl_screencast_socket = x11
                .get_property(
                    false,
                    selection_owner,
                    egl_screencast_socket_atom?,
                    utf8_string_atom,
                    0,
                    1024,
                )
                .await?
                .reply()
                .await?;
            if egl_screencast_socket.type_ == x11rb::NONE {
                return Err(anyhow::anyhow!("No socket found"));
            }
            (egl_screencast_cookie.value, egl_screencast_socket.value)
        } else {
            return Err(anyhow::anyhow!("No compatible compositor found"));
        };

        let path = std::ffi::OsStr::from_bytes(&path);
        println!("path: {:?} {}", path, std::str::from_utf8(&cookie)?);
        let socket = std::path::Path::new(path);
        let conn = Async::new(UnixStream::connect(socket)?)?;

        Ok(Self {
            conn,
            cookie: String::from_utf8(cookie).context("Invalid cookie")?,
            buf: Vec::new(),
            len: None,
            pos: 0,
            out_buf: Vec::new(),
            out_pos: 0,
        })
    }
}

#[derive(Debug)]
struct Session {
    path: zbus::zvariant::ObjectPath<'static>,
    source_type: SourceType,
    allow_multiple: bool,
    cursor_mode: CursorMode,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl Session {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
    #[zbus(signal)]
    async fn closed(signal_ctx: zbus::SignalContext<'_>) -> zbus::Result<()>;
    async fn close(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(signal_context)] signal_ctx: zbus::SignalContext<'_>,
    ) -> zbus::fdo::Result<()> {
        Self::closed(signal_ctx).await?;
        server.remove::<Self, _>(&self.path).await.unwrap();
        Ok(())
    }
}

#[derive(Debug)]
struct ScreenCast {
    sessions: std::collections::HashMap<String, Session>,
    /// The monitor to pick when the requester doesn't support allow_multiple.
    default_monitor: AtomicUsize,
}

struct ArrayExtend<'a>(zbus::zvariant::Array<'a>);

impl<'a, T: Into<zvariant::Value<'a>>> Extend<T> for ArrayExtend<'a> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for item in iter {
            self.0.append(item.into()).unwrap();
        }
    }
}

impl<'a, T: zvariant::Type + Into<zvariant::Value<'a>>> FromIterator<T> for ArrayExtend<'a> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut array = Self(zvariant::Array::new(T::signature()));
        array.extend(iter);
        array
    }
}
impl<'a> From<ArrayExtend<'a>> for zvariant::Value<'a> {
    fn from(value: ArrayExtend<'a>) -> Self {
        zvariant::Value::Array(value.0)
    }
}
impl<'a> TryFrom<ArrayExtend<'a>> for zvariant::OwnedValue {
    type Error = zvariant::Error;
    fn try_from(value: ArrayExtend<'a>) -> Result<Self, Self::Error> {
        zvariant::Value::Array(value.0).try_into()
    }
}

bitflags::bitflags! {
    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    struct CursorMode: u32 {
        const None = 0;
        const Hidden = 1;
        const Embedded = 2;
        const Metadata = 4;
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    struct SourceType: u32 {
        const Monitor = 1;
        const Window = 2;
        const Virtual = 4;
    }
}
impl Type for SourceType {
    fn signature() -> zvariant::Signature<'static> {
        u32::signature()
    }
}

impl Type for CursorMode {
    fn signature() -> zvariant::Signature<'static> {
        u32::signature()
    }
}

struct FdoError<'a, T>(&'a str, T);
impl<'a, T> FdoError<'a, T> {
    fn with_msg(msg: &'a str) -> impl FnOnce(T) -> Self {
        move |e| Self(msg, e)
    }
}
impl<'a, T: std::fmt::Debug> From<FdoError<'a, T>> for zbus::fdo::Error {
    fn from(FdoError(msg, e): FdoError<T>) -> Self {
        zbus::fdo::Error::Failed(format!("{msg}: {e:?}"))
    }
}

#[derive(SerializeDict, Type)]
#[zvariant(signature = "a{sv}")]
struct StreamDict {
    position: (i32, i32),
    size: (i32, i32),
    source_type: SourceType,
    mapping_id: Option<String>,
}

#[derive(SerializeDict, Type)]
#[zvariant(signature = "a{sv}")]
struct StartResponse {
    streams: Vec<(u32, StreamDict)>,
}

#[derive(DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
struct SelectSourcesOptions {
    multiple: Option<bool>,
    types: Option<SourceType>,
    cursor_mode: Option<CursorMode>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    const AVAILABLE_SOURCE_TYPE: SourceType = SourceType::Monitor.union(SourceType::Virtual);
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        5
    }
    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        (CursorMode::Embedded | CursorMode::Hidden).bits()
    }
    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        Self::AVAILABLE_SOURCE_TYPE.bits()
    }

    #[zbus(out_args("response", "results"))]
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        _handle: zbus::zvariant::ObjectPath<'_>,
        session_handle: zbus::zvariant::ObjectPath<'_>,
        _app_id: &str,
        _options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::fdo::Result<(
        u32,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    )> {
        server
            .at(
                &session_handle,
                Session {
                    path: session_handle.to_owned(),
                    allow_multiple: false,
                    source_type: SourceType::Monitor,
                    cursor_mode: CursorMode::Hidden,
                },
            )
            .await?;
        Ok((0, Default::default()))
    }

    #[zbus(out_args("response", "results"))]
    async fn select_sources(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        _handle: zbus::zvariant::ObjectPath<'_>,
        session_handle: zbus::zvariant::ObjectPath<'_>,
        _app_id: &str,
        options: SelectSourcesOptions,
    ) -> zbus::fdo::Result<(
        u32,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    )> {
        let session = server.interface::<_, Session>(session_handle).await?;
        let mut session = session.get_mut().await;
        if let Some(source_type) = options.types {
            session.source_type = source_type & Self::AVAILABLE_SOURCE_TYPE;
            if session.source_type.is_empty() {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "Invalid source type".to_string(),
                ));
            }
        }
        if let Some(allow_multiple) = options.multiple {
            session.allow_multiple = allow_multiple;
        }
        if let Some(cursor_mode) = options.cursor_mode {
            session.cursor_mode = cursor_mode;
        }
        // TODO(yshui): handle `cursor_mode`
        Ok((0, Default::default()))
    }

    #[zbus(out_args("response", "results"))]
    async fn start(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        _handle: zbus::zvariant::ObjectPath<'_>,
        session_handle: zbus::zvariant::ObjectPath<'_>,
        _app_id: &str,
        _parent_window: &str,
        _options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::fdo::Result<(u32, StartResponse)> {
        let session = server.interface::<_, Session>(session_handle).await?;
        let (x11, screen, fut) = x11rb_async::rust_connection::RustConnection::connect(None)
            .await
            .map_err(FdoError::with_msg("Failed to connect to X11"))?;
        let _task = smol::spawn(fut);
        let mut picom = Picom::new(&x11, screen)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let root = x11.setup().roots[screen].root;
        let cookie = picom.cookie.clone();
        let session = session.get_mut().await;
        let source_type = if !session.allow_multiple {
            session
                .source_type
                .iter()
                .next()
                .unwrap_or(SourceType::Monitor)
        } else {
            session.source_type
        };
        let mut rectangles = SmallVec::<[_; 6]>::new();
        let mut types = SmallVec::<[_; 6]>::new();
        if source_type.contains(SourceType::Monitor) {
            let monitors = x11
                .randr_get_monitors(root, true)
                .await
                .map_err(FdoError::with_msg("Failed to get monitors"))?
                .reply()
                .await
                .map_err(FdoError::with_msg("Failed to get monitors"))?;
            let monitor_count = monitors.monitors.len();
            let mut m = monitors.monitors.iter().map(|m| protocol::Rectangle {
                x: m.x as i32,
                y: m.y as i32,
                width: m.width as u32,
                height: m.height as u32,
            });
            if session.allow_multiple {
                rectangles.extend(m)
            } else {
                let old_default_monitor = self
                    .default_monitor
                    .load(std::sync::atomic::Ordering::Relaxed);
                let mut default_monitor = old_default_monitor;
                if default_monitor >= monitor_count {
                    default_monitor = 0;
                }
                tracing::info!("default_monitor: {}", default_monitor);
                rectangles.extend(m.nth(default_monitor));
                let _ = self.default_monitor.compare_exchange(
                    old_default_monitor,
                    default_monitor + 1,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                );
            };
            types.extend(rectangles.iter().map(|_| SourceType::Monitor));
        }
        if source_type.contains(SourceType::Virtual)
            && (session.allow_multiple || rectangles.is_empty())
        {
            let root = x11.setup().roots[screen].root;
            let geom = x11
                .get_geometry(root)
                .await
                .map_err(FdoError::with_msg("root geometry"))?
                .reply()
                .await
                .map_err(FdoError::with_msg("root geometry"))?;
            rectangles.push(protocol::Rectangle {
                x: 0,
                y: 0,
                width: geom.width as _,
                height: geom.height as _,
            });
            types.push(SourceType::Virtual);
        }
        picom
            .send(protocol::ClientMessage::CreateStream {
                cookie,
                rectangles: rectangles.clone(),
                embed_cursor: session.cursor_mode.contains(CursorMode::Embedded),
            })
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let node_ids = match picom
            .next()
            .await
            .unwrap()
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
        {
            protocol::ServerMessage::StreamCreated { node_ids } => node_ids,
            protocol::ServerMessage::StreamCreationError { error } => {
                return Err(zbus::fdo::Error::Failed(error))
            }
        };
        // We pretend the virtual source is a monitor source
        let streams = node_ids
            .into_iter()
            .zip(types)
            .zip(rectangles)
            .map(|((node_id, type_), rectangle)| {
                (
                    node_id,
                    StreamDict {
                        position: (rectangle.x, rectangle.y),
                        size: (rectangle.width as i32, rectangle.height as i32),
                        source_type: type_,
                        mapping_id: None,
                    },
                )
            })
            .collect();

        Ok((0, StartResponse { streams }))
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let screen_cast = ScreenCast {
        sessions: Default::default(),
        default_monitor: AtomicUsize::new(0),
    };
    let zbus = zbus::connection::Builder::session()?
        .name("org.freedesktop.impl.portal.desktop.picom")?
        .serve_at("/org/freedesktop/portal/desktop", screen_cast)?;
    let (_tx, zbus_cancel) = oneshot::channel::<()>();
    smol::block_on(async move {
        let _conn = zbus.build().await.unwrap();
        zbus_cancel.await.unwrap();
    });

    Ok(())
}
