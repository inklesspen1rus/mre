use core::time::Duration;

use axum::{
    body::Bytes, extract::{ws::WebSocket, WebSocketUpgrade}, response::Response, routing::any, Router
};
use futures::{future::{self, join}, select, FutureExt, SinkExt, StreamExt};

#[tokio::main]
async fn main() {
    let r = Router::new().route("/", any(handler));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    axum::serve(listener, r).await.unwrap();
}

async fn handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(socket: WebSocket) {

    let (mut sink, stream) = socket.split();

    let reader = async {
        stream.for_each(|_| future::ready(())).await;
    };
    let writer = async {
        let bytes = Bytes::from_owner(vec![0u8; 4096]);
        for x in std::iter::repeat(bytes) {
            sink.send(axum::extract::ws::Message::Binary(x)).await?;
            tokio::time::sleep(Duration::from_secs_f32(0.01)).await;
        }
        Result::<(), axum::Error>::Ok(())
    };

    let main_task = async {
        _ = futures::join!(reader, writer);
    }.fuse();

    let mut main_task = std::pin::pin!(main_task);

    // 45 - really MAGIC number for my machine.
    // 10 will NOT produce the issue.
    // 60 will NOT produce the issue.
    // 45 - will. IDK why.
    let timeout = tokio::time::sleep(Duration::from_secs(45)).fuse();
    let mut timeout = std::pin::pin!(timeout);

    futures::select! {
        _ = timeout => (),
        _ = main_task => (),
    };
}
