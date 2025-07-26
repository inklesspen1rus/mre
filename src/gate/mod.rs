use crate::gate::asset::{Asset, Assets, MarketType};
use crate::gate::{error::ExchangeError};
use crate::gate::utils::{GateExchangeUtils, normalize_symbol};
use futures::{StreamExt, join};
use ntex::connect::rustls::TlsConnector;
use ntex::ws;
use rustls::RootCertStore;
use core::time::Duration;
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};
use crate::http::DynamicIterator;

pub mod asset;
mod error;
pub mod utils;

macro_rules! from_headers {
    ($headers:expr) => {
        &mut $headers
            .iter()
            .map(|(k, v)| (k as &dyn AsRef<str>, v as &dyn AsRef<str>))
            as DynamicIterator<(&dyn AsRef<str>, &dyn AsRef<str>)>
    };
}

pub struct Gate {
    ws_url: String,
    market: MarketType,
}

static ASSETS: LazyLock<Mutex<Assets>> = LazyLock::new(|| Mutex::new(Assets::new()));

async fn fetch_assets_by(
    market: MarketType,
    utils: Rc<GateExchangeUtils>,
) -> Result<Vec<Asset>, ExchangeError> {
    let headers: HashMap<String, String> = HashMap::new();

    let url = if let MarketType::Spot = market {
        "https://api.gateio.ws/api/v4/spot/currency_pairs"
    } else {
        "https://fx-api.gateio.ws/api/v4/futures/usdt/contracts"
    };

    let response = utils
                    .http_client
                    .request(
                        "GET".to_string(),
                        url.to_string(),
                        from_headers!(headers),
                        None,
                    )
                    .await
                    .map_err(|e| ExchangeError::ApiError(format!("Erro ao buscar ativos: {e}")))?
                    .body()
                    .limit(100 * 1024 * 1024)
                    .await
                    .map_err(|e| ExchangeError::ApiError(format!("Corpo inválido: {e}")))?;

    let response = std::str::from_utf8(&response)
        .map_err(|e| ExchangeError::ApiError(format!("Resposta inválida: {e:?}")))?;

    let json: serde_json::Value =
        serde_json::from_str(response).map_err(ExchangeError::JsonError)?;

    let mut assets = Vec::new();

    if let Some(symbols) = json.as_array() {
        for symbol in symbols {
            if let MarketType::Future = market {
                if symbol.get("status").and_then(|v| v.as_str()) != Some("trading") {
                    continue;
                }
            } else if symbol.get("trade_status").and_then(|v| v.as_str()) != Some("tradable") {
                continue;
            }

            let name;

            if let MarketType::Future = market {
                name = symbol
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
            } else {
                name = symbol
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
            }

            let mut parts = name.splitn(2, '_');

            let base = parts.next().unwrap_or("");
            let quote = parts.next().unwrap_or("");

            let symbol_name;

            if let MarketType::Spot = market {
                symbol_name = format!("{base}/{quote}");
            } else {
                symbol_name = format!("{base}/{quote}:{quote}");
            }

            assets.push(Asset {
                symbol: symbol_name,
                base: base.to_string(),
                quote: quote.to_string(),
                market: market.clone(),
                exchange: "Gate".to_string(),
            });
        }
    }

    Ok(assets)
}

pub async fn fetch_assets(utils: Rc<GateExchangeUtils>) -> Result<Assets, ExchangeError> {
    let (spot, future) = join!(
        fetch_assets_by(MarketType::Spot, utils.clone()),
        fetch_assets_by(MarketType::Future, utils)
    );

    let (spot, future) = (spot?, future?);

    let spot = spot
        .into_iter()
        .map(|asset| (asset.symbol.clone(), asset))
        .collect();

    let future = future
        .into_iter()
        .map(|asset| (asset.symbol.clone(), asset))
        .collect();

    let mut lock = ASSETS.lock().unwrap();

    lock.spot = spot;
    lock.future = future;

    Ok(lock.clone())
}

impl Gate {
    pub fn new(ws_url: String, market: MarketType) -> Self {
        Self {
            ws_url,
            market,
        }
    }

    pub async fn watch(&self, symbol: String) -> Result<(), Box<dyn Error>> {
        let normalized_symbol = normalize_symbol(&symbol);

        let connector = {
            let root_store = RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.into(),
            };

            let config = rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();

            TlsConnector::new(config)
        };

        let ws_client = ws::WsClient::build(&self.ws_url)
            .connector(connector)
            .finish()
            .map_err(|e| format!("Build error: {e:?}"))?
            .connect()
            .await
            .map_err(|e| format!("Connect error: {e:?}"));

        let seal = ws_client?.seal();
        let sink = seal.sink();

        // Subscribe
        let msg = serde_json::json!({
          "time": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
          "channel": if self.market == MarketType::Spot { "spot.order_book_update" } else { "futures.order_book_update" },
          "event": "subscribe",
          "payload": [&normalized_symbol, "100ms"]
        });
        sink.send(ws::Message::Text(msg.to_string().into())).await?;

        let sink2 = sink.clone();

        ntex::rt::spawn(async move {
            let mut rx_ws = seal.receiver();

            loop {
                match rx_ws.next().await {
                    Some(Ok(ws::Frame::Ping(p))) => {
                        _ = sink2.send(ws::Message::Pong(p)).await.unwrap();
                    }
                    Some(Err(e)) => {
                        eprintln!("WebSocket Error: {:?}", e);
                        break;
                    }
                    _ => {}
                }
            }

            println!("WebSocket for {} disconnected.", normalized_symbol);
        });

        Ok(())
    }
}
