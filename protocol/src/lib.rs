/// Client-server protocol uses length delimited JSON.
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rectangle {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    // The cookie is here to prevent "privilege escalation". All clients that are connected
    // to the same X server already has the ability to capture the screen. We just try not to
    // expand that privilege to other clients. So we set a cookie on one of our windows and
    // only clients that know the cookie can create streams, this way we can be sure that the
    // client is connected to the same X server as us.
    /// Create a new stream.
    CreateStream {
        cookie: String,
        rectangles: SmallVec<[Rectangle; 6]>,
        embed_cursor: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    StreamCreated { node_ids: SmallVec<[u32; 6]> },
    StreamCreationError { error: String },
}
