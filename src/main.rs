use std::rc::Rc;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use rustls::crypto::aws_lc_rs::default_provider;
use crate::gate::asset::MarketType;
use crate::gate::{fetch_assets, Gate};
use crate::gate::utils::GateExchangeUtils;
use crate::http::NtexHttpClient;

pub mod gate;
mod http;

async fn reenter(gate: &Gate, symbol: String) -> String {
  let _ = gate.watch(symbol.clone()).await;
  symbol
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
  tracing_subscriber::fmt::init();

  default_provider()
    .install_default()
    .expect("Failed to install default CryptoProvider");

  let utils = Rc::new(GateExchangeUtils::new(NtexHttpClient::new()));

  let gate = Gate::new("wss://fx-ws.gateio.ws/v4/ws/usdt".to_string(), MarketType::Future);

  let assets = fetch_assets(utils).await.unwrap();

  let mut tasks = FuturesUnordered::new();

  for (symbol, _) in assets.future.iter() {
    tasks.push(reenter(&gate, symbol.clone()));
  }

  println!("{}", tasks.len());

  while let Some(symbol) = tasks.next().await {
    tasks.push(reenter(&gate, symbol));
  }

  return Ok(())
}