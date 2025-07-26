use core::error::Error;

use futures::StreamExt;
use ntex::ws;

async fn spawn_worker() -> Result<(), Box<dyn Error>> {
    let ws_client = ntex::ws::WsClient::build("ws://127.0.0.1:3000")
        .finish()
        .map_err(|e| format!("Build error: {e:?}"))?
        .connect()
        .await
        .map_err(|e| format!("Connect error: {e:?}"))?;

    let seal = ws_client.seal();
    let sink = seal.sink();

    ntex::rt::spawn(async move {
        let mut rx_ws = seal.receiver();

        while let Some(x) = rx_ws.next().await {
            match x {
                Ok(ws::Frame::Ping(p)) => {
                    _ = sink.send(ws::Message::Pong(p)).await.unwrap();
                }
                Err(e) => {
                    eprintln!("WebSocket Error: {:?}", e);
                    break;
                }
                _ => {}
            }
        }

        println!("WebSocket for disconnected.");
    });

    Ok(())
}

async fn spawn_spawn_worker() {
    loop {
        _ = spawn_worker().await.inspect_err(|e| eprintln!("{e}"));
    }
}

#[compio::main]
async fn main() {
    // 600 - really MAGIC number for my machine.
    // 100 will NOT produce the issue.
    // 5500 will NOT produce the issue.
    // 600 - will. IDK why.
    let futs = std::iter::from_fn(|| Some(spawn_spawn_worker()))
        .take(600)
        .collect::<Vec<_>>();

    futures::future::join_all(futs).await;
}
