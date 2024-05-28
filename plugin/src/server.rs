use async_channel::{Receiver, Sender};
use fs4::FileExt as _;
use futures_util::{
    io::{ReadHalf, WriteHalf},
    stream::FuturesOrdered,
    AsyncReadExt, AsyncWriteExt as _, Sink, SinkExt as _, Stream, StreamExt as _, TryFutureExt,
};
use smallvec::SmallVec;
use smol::Async;
use std::{
    io::{Read as _, Write as _},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    pin::pin,
    sync::Arc,
};
use x11rb::protocol::xproto::{ConnectionExt as _, PropMode};

fn place_runtime_file(name: &str) -> PathBuf {
    if let Some(path) = xdg::BaseDirectories::with_prefix("picom")
        .ok()
        .and_then(|base| base.place_runtime_file(name).ok())
    {
        return path;
    }
    let name = format!("picom-{}", name);
    if let Some(tmp) = std::env::var_os("TMPDIR") {
        Path::new(&tmp).join(name)
    } else {
        Path::new("/tmp").join(name)
    }
}
use protocol::{ClientMessage, ServerMessage};

#[pin_project::pin_project]
struct Client {
    #[pin]
    tx: Sender<ServerMessage>,
    #[pin]
    rx: Receiver<anyhow::Result<ClientMessage>>,

    read_task: smol::Task<()>,
}

impl Client {
    async fn read_side_inner(
        mut stream: ReadHalf<Async<UnixStream>>,
        tx: &Sender<anyhow::Result<ClientMessage>>,
    ) -> anyhow::Result<()> {
        let mut buf = Vec::new();
        loop {
            let mut len = [0u8; 4];
            match stream.read_exact(&mut len).await {
                Ok(()) => (),
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        return Ok(());
                    }
                    return Err(e.into());
                }
            }
            let len = u32::from_be_bytes(len);
            tracing::info!("Read message of length {}", len);
            buf.resize(len as usize, 0u8);
            stream.read_exact(&mut buf).await?;
            let msg = serde_json::from_slice(&buf)?;
            tx.send(Ok(msg)).await?;
        }
    }
    async fn read_side(
        stream: ReadHalf<Async<UnixStream>>,
        tx: Sender<anyhow::Result<ClientMessage>>,
    ) {
        match Self::read_side_inner(stream, &tx).await {
            Ok(()) => (),
            Err(e) => {
                tracing::error!("Client read side error: {:?}", e);
                tx.send(Err(e)).await.ok();
            }
        }
        tracing::info!("Client read side exited");
    }
    async fn write_side(
        mut stream: WriteHalf<Async<UnixStream>>,
        rx: Receiver<ServerMessage>,
    ) -> anyhow::Result<()> {
        loop {
            let Ok(msg) = rx.recv().await else {
                break Ok(());
            };
            let msg = serde_json::to_string(&msg)?;
            let msg = msg.as_bytes();
            let mut len = [0u8; 4];
            len.copy_from_slice(&(msg.len() as u32).to_be_bytes());
            stream.write_all(&len).await?;
            stream.write_all(msg).await?;
        }
    }
    fn new(stream: Async<UnixStream>) -> Self {
        let (tx1, rx1) = async_channel::unbounded();
        let (tx2, rx2) = async_channel::unbounded();
        let (read, write) = stream.split();
        let read_task = smol::spawn(Self::read_side(read, tx2));
        smol::spawn(async {
            Self::write_side(write, rx1)
                .unwrap_or_else(|e| {
                    tracing::error!("Client write side error: {:?}", e);
                })
                .await;
        })
        .detach();

        Self {
            tx: tx1,
            rx: rx2,
            read_task,
        }
    }
}

impl Stream for Client {
    type Item = anyhow::Result<ClientMessage>;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.project();
        if this.rx.is_closed() {
            return std::task::Poll::Ready(None);
        }
        this.rx.poll_next(cx)
    }
}

impl Sink<ServerMessage> for Client {
    type Error = anyhow::Error;
    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        if self.tx.is_closed() {
            return std::task::Poll::Ready(Err(anyhow::anyhow!("Channel closed")));
        }
        std::task::Poll::Ready(Ok(()))
    }
    fn start_send(self: std::pin::Pin<&mut Self>, item: ServerMessage) -> Result<(), Self::Error> {
        let this = self.project();
        match this.tx.try_send(item) {
            Ok(_) => Ok(()),
            Err(async_channel::TrySendError::Closed(_)) => Err(anyhow::anyhow!("Channel closed")),
            Err(async_channel::TrySendError::Full(_)) => unreachable!(),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        if self.tx.is_closed() {
            return std::task::Poll::Ready(Err(anyhow::anyhow!("Channel closed")));
        }
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.tx.close();
        std::task::Poll::Ready(Ok(()))
    }
}

async fn client_task_inner(
    our_cookie: Arc<String>,
    stream: Async<UnixStream>,
    pw_tx: &crate::PipewireSender,
) -> anyhow::Result<()> {
    let client = Box::pin(Client::new(stream));
    let mut client = client.fuse();
    tracing::info!("Client task started");
    let Some(msg) = client.next().await else {
        return Ok(());
    };
    let ClientMessage::CreateStream { cookie, rectangles, embed_cursor } = msg?;
    tracing::info!("CreateStream: {:?}", cookie);
    if cookie != *our_cookie {
        return Err(anyhow::anyhow!("Invalid cookie {}", our_cookie));
    }
    let rxs = smol::unblock({
        let pw_tx = pw_tx.clone();
        move || {
            let mut pw_tx = pw_tx.start_send();
            let mut rxs = FuturesOrdered::new();
            for r in rectangles {
                let (tx, rx) = oneshot::channel();
                pw_tx.send(crate::MessagesToPipewire::CreateStream {
                    width: r.width,
                    height: r.height,
                    x: r.x,
                    y: r.y,
                    embed_cursor,
                    reply: tx,
                })?;
                rxs.push_back(rx);
            }
            Ok::<_, anyhow::Error>(rxs)
        }
    })
    .await?;

    let node_ids: SmallVec<[_; 6]> = rxs.collect().await;
    let node_ids: SmallVec<[_; 6]> = node_ids.into_iter().collect::<Result<_, _>>()?;
    let node_ids: Result<SmallVec<[_; 6]>, _> = node_ids.into_iter().collect();
    tracing::info!("CreateStream reply: {:?}", node_ids);
    match node_ids {
        Ok(node_ids) => {
            client
                .send(ServerMessage::StreamCreated { node_ids })
                .await?;
        }
        Err(e) => {
            client
                .send(ServerMessage::StreamCreationError {
                    error: format!("{:?}", e),
                })
                .await?;
        }
    }
    tracing::info!("Client task exited");
    Ok(())
}
async fn client_task(
    our_cookie: Arc<String>,
    stream: Async<UnixStream>,
    pw_tx: crate::PipewireSender,
) {
    match client_task_inner(our_cookie, stream, &pw_tx).await {
        Ok(_) => (),
        Err(e) => println!("Error: {:?}", e),
    }
}

async fn run(
    our_cookie: Arc<String>,
    pw_tx: crate::PipewireSender,
    selection_owner: u32,
) -> anyhow::Result<()> {
    let file_name = format!("egl-screencast-{}", std::env::var("DISPLAY")?);
    let mut pidfile = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(place_runtime_file(&format!("{}.pid", file_name)))?;
    match pidfile.try_lock_exclusive() {
        Ok(_) => {
            pidfile.write_all(format!("{}", std::process::id()).as_bytes())?;
        }
        Err(e) => {
            let mut buf = String::new();
            pidfile.read_to_string(&mut buf)?;
            eprintln!(
                "Another instance of picom-egl-screencast is running: {}, pid: {}",
                e, buf
            );
            return Ok(());
        }
    }
    let socket_path = place_runtime_file(&file_name);
    std::fs::remove_file(&socket_path).or_else(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(e)
        }
    })?;

    {
        let (x11, _) = x11rb::rust_connection::RustConnection::connect(None)?;
        let egl_screencast_socket_atom = x11
            .intern_atom(false, b"EGL_SCREENCAST_SOCKET")?
            .reply()?
            .atom;
        let utf8_string = x11.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let path_bytes = socket_path.to_str().unwrap().as_bytes();
        x11.change_property(
            PropMode::REPLACE,
            selection_owner,
            egl_screencast_socket_atom,
            utf8_string,
            8,
            path_bytes.len() as u32,
            path_bytes,
        )?
        .check()?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    let listener = async_io::Async::new(listener)?;
    let mut incoming = pin!(listener.incoming().fuse());
    println!("Listening on {:?}", listener.get_ref().local_addr()?);
    while let Some(new_client) = incoming.next().await {
        match new_client {
            Ok(new_client) => {
                println!("New client from {:?}", new_client);
                smol::spawn(client_task(our_cookie.clone(), new_client, pw_tx.clone())).detach();
            }
            Err(e) => tracing::error!("Error accepting new client: {:?}", e),
        }
    }

    Ok(())
}
pub fn start_server(our_cookie: Arc<String>, pw_tx: crate::PipewireSender, selection_owner: u32) {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    smol::block_on(async {
        match run(our_cookie, pw_tx, selection_owner).await {
            Ok(_) => println!("Server exited."),
            Err(e) => {
                println!("Error: {:?}", e);
            }
        }
    });
}
