use crate::gate::coditional_future::ConditionalFuture;
use crate::http::{DynamicIterator, NtexHttpClient};
use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::mem;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use futures::stream::FuturesUnordered;
use futures::{join, StreamExt};
use ntex::connect::rustls::TlsConnector;
use ntex::ws;
use once_cell::sync::OnceCell;
use ratelimit::Ratelimiter;
use rust_decimal::Decimal;
use rustls::RootCertStore;
use serde::Deserialize;
use crate::gate::asset::{Asset, Assets, MarketType};
use crate::gate::error::ExchangeError;
use crate::gate::order::{OrderBook, OrderBookUpdate};
use crate::gate::utils::{normalize_symbol, GateExchangeUtils};
use crate::project;

pub mod asset;
pub mod utils;
mod order;
mod error;
mod coditional_future;
pub mod sync;

macro_rules! from_headers {
  ($headers:expr) => {
    &mut $headers
      .iter()
      .map(|(k, v)| (k as &dyn AsRef<str>, v as &dyn AsRef<str>))
    as DynamicIterator<(&dyn AsRef<str>, &dyn AsRef<str>)>
  };
}

#[derive(Debug, Deserialize)]
struct SpotDepthSnapshot {
  id: u64,
  bids: Vec<(Decimal, Decimal)>,
  asks: Vec<(Decimal, Decimal)>,
}

#[derive(Debug, Deserialize)]
struct FutureDepthSnapshotItem {
  p: Decimal,
  s: Decimal,
}

#[derive(Debug, Deserialize)]
struct FutureDepthSnapshot {
  id: u64,
  bids: Vec<FutureDepthSnapshotItem>,
  asks: Vec<FutureDepthSnapshotItem>,
}

type Init = Rc<RefCell<HashMap<String, Vec<OrderBookUpdate>>>>;
pub type SharedBook = Rc<RefCell<OrderBook>>;

pub struct Gate {
  ws_url: String,
  market: MarketType,
  utils: Rc<GateExchangeUtils>,
  subscribed: Rc<RefCell<HashMap<String, (SharedBook, ws::WsSink)>>>,
  init_queue: Init,
}

static ASSETS: LazyLock<Mutex<Assets>> = LazyLock::new(|| Mutex::new(Assets::new()));

pub static CONNECT_LIMITER: OnceCell<Arc<Ratelimiter>> = OnceCell::new();
pub static HTTP_LIMITER: OnceCell<Arc<Ratelimiter>> = OnceCell::new();

async fn fetch_assets_by(market: MarketType, utils: Rc<GateExchangeUtils>) -> Result<Vec<Asset>, ExchangeError> {
  let headers: HashMap<String, String> = HashMap::new();

  let url = if let MarketType::Spot = market {
    "https://api.gateio.ws/api/v4/spot/currency_pairs"
  } else {
    "https://fx-api.gateio.ws/api/v4/futures/usdt/contracts"
  };

  let response = loop {
    match HTTP_LIMITER
      .get()
      .expect("Limiter not initialized")
      .try_wait()
    {
      Ok(()) => {
        break utils
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
      }
      Err(duration) => {
        ntex::time::sleep(duration).await;
      }
    }
  };

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

type HistoryBids = Rc<RefCell<HashSet<Decimal>>>;

impl Gate {
  pub fn new(ws_url: String, market: MarketType, utils: Rc<GateExchangeUtils>) -> Self {
    Self {
      ws_url,
      market,
      utils,
      subscribed: Rc::new(RefCell::new(HashMap::new())),
      init_queue: Rc::new(RefCell::new(HashMap::new())),
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

    let book = Rc::new(RefCell::new(OrderBook::default()));
    self.subscribed.borrow_mut().insert(normalized_symbol.clone(), (book, sink));

    let utils = self.utils.clone();
    let market = self.market.clone();
    let subscribed = self.subscribed.clone();
    let init_queue = self.init_queue.clone();

    ntex::rt::spawn(async move {
      let mut rx_ws = seal.receiver();

      let mut client_tasks = FuturesUnordered::new();

      let history_bids = HistoryBids::default();

      loop {
        macros::select! {
          message = rx_ws.next() => match message {
            Some(Ok(ws::Frame::Text(bytes))) => {
              let str = String::from_utf8(bytes.to_vec());
              let task = Box::pin(Self::handle_message(str.ok().unwrap(), subscribed.clone(), history_bids.clone(), init_queue.clone(), utils.clone(), market.clone()));

              let task_projection = project!(task);

              if task_projection.is_err() {
                client_tasks.push(task_projection.err().unwrap());
              }
            }
            Some(Ok(ws::Frame::Ping(p))) => {
              let s = subscribed.borrow();
              if let Some((_, sink)) = s.get(&normalized_symbol) {
                let _ = sink.send(ws::Message::Pong(p)).await;
              }
            }
            Some(Err(e)) => {
              eprintln!("WebSocket Error: {:?}", e);
              break;
            }
            _ => {}
          },
          client_res = client_tasks.next(), if !client_tasks.is_empty() => {
            if client_res.is_none() {
              break;
            }
          }
        }
      }

      subscribed.borrow_mut().remove(&normalized_symbol);
      println!("WebSocket for {} disconnected.", normalized_symbol);
    });

    Ok(())
  }

  async fn handle_message(
    text: String,
    subscribed: Rc<RefCell<HashMap<String, (SharedBook, ws::WsSink)>>>,
    history_bids: HistoryBids,
    init: Init,
    utils: Rc<GateExchangeUtils>,
    market: MarketType,
  ) -> Option<()> {
    if let Ok(parsed_value) = serde_json::from_str::<serde_json::Value>(&text) {
      if parsed_value.get("event").and_then(|e| e.as_str()) == Some("update") {
        let result = parsed_value.get("result").unwrap();
        let symbol = result["s"].as_str().unwrap().to_string();

        let (book, _) = {
          let s = subscribed.borrow();
          let (book, sink) = s.get(&symbol).unwrap();
          (book.clone(), sink.clone())
        };

        let update = Self::build_update(result, &market).unwrap();

        if book.borrow().update_id == 0 { // First update
          if update.full {
            book.borrow_mut().apply_update(&update, &mut vec![]);
            return Some(());
          }

          book.borrow_mut().update_id = 1;
          init.borrow_mut().entry(symbol.to_string()).or_default();

          let snapshot =
            Self::fetch_snapshot(&symbol, utils.clone(), update.first_update_id, market.clone()).await;

          if snapshot.is_err() {
            book.borrow_mut().update_id = 0;
            return Some(());
          }

          let snapshot = snapshot.ok()?;

          let mut retries = 0;
          let processed = loop {
            if retries == 5 {
              book.borrow_mut().update_id = 0;
              return Some(());
            }

            let mut pending = mem::take(init.borrow_mut().get_mut(&symbol)?);

            let idx = pending.iter().position(|item| {
              item.first_update_id <= snapshot.update_id + 1
                && item.last_update_id > snapshot.update_id
            });

            if idx.is_none() {
              ntex::time::sleep(Duration::from_millis(100)).await;
              retries += 1;
              continue;
            }

            pending.drain(0..idx.unwrap());

            break pending;
          };

          let mut book_bm = book.borrow_mut();
          book_bm.asks = snapshot.asks;
          book_bm.bids = snapshot.bids;
          book_bm.update_id = snapshot.update_id;

          let mut history_bm = history_bids.borrow_mut();

          for (price, qty) in &book_bm.bids {
            if qty.eq(&Decimal::ZERO) {
              history_bm.remove(&price.0);
            } else {
              history_bm.insert(price.0);
            }
          }

          let mut updates = vec![];
          for update in processed {
            if update.first_update_id > book_bm.update_id + 1
              || update.last_update_id < book_bm.update_id + 1
            {
              book_bm.update_id = 0;
              break;
            }
            book_bm.apply_update(&update, &mut updates);
            for (price, qty) in &update.bids {
              if qty.eq(&Decimal::ZERO) {
                history_bm.remove(&price);
              } else {
                history_bm.insert(*price);
              }
            }
          };
        } else if book.borrow().update_id == 1 { // Initializing
          init.borrow_mut().get_mut(&symbol).unwrap().push(update);
        } else { // Normal update
          {
            let mut book_mut = book.borrow_mut();
            if update.first_update_id > book_mut.update_id + 1 || update.last_update_id < book_mut.update_id + 1 {
              book_mut.asks.clear();
              book_mut.bids.clear();
              book_mut.update_id = 0;
              return Some(());
            }
          }

          book.borrow_mut().apply_update(&update, &mut vec![]);

          let mut history_bm = history_bids.borrow_mut();
          for (price, qty) in &update.bids {
            if qty.eq(&Decimal::ZERO) {
              history_bm.remove(&price);
            } else {
              history_bm.insert(*price);
            }
          }

          for (r_bid, _) in book.borrow().bids.iter() {
            if history_bm.get(&r_bid.0).is_none() {
              panic!("Error in BID price {:?}", r_bid);
            }
          }
        }
      }
    }
    Some(())
  }

  fn build_update(parsed: &serde_json::Value, market: &MarketType) -> Option<OrderBookUpdate> {
    let parse_side = |side: &serde_json::Value| -> Option<Vec<(Decimal, Decimal)>> {
      side.as_array()?.iter().map(|entry| {
        if *market == MarketType::Spot {
          let price = entry[0].as_str()?.parse().ok()?;
          let qty = entry[1].as_str()?.parse().ok()?;
          Some((price, qty))
        } else {
          let item: FutureDepthSnapshotItem = serde_json::from_value(entry.clone()).ok()?;
          Some((item.p, item.s))
        }
      }).collect()
    };

    Some(OrderBookUpdate {
      asks: parse_side(&parsed["a"])?,
      bids: parse_side(&parsed["b"])?,
      first_update_id: parsed["U"].as_u64()?,
      last_update_id: parsed["u"].as_u64()?,
      full: parsed["full"].as_bool().unwrap_or(false),
    })
  }

  async fn fetch_snapshot(
    symbol: &str,
    utils: Rc<GateExchangeUtils>,
    initial_event_u: u64,
    market: MarketType,
  ) -> Result<OrderBook, Box<dyn Error>> {
    let mut processed = OrderBook::default();

    let mut retries = 0;

    while processed.update_id < initial_event_u {
      if retries == 5 {
        return Err("Max retry".into());
      }

      let uri = match market {
        MarketType::Spot => {
          format!(
            "https://api.gateio.ws/api/v4/spot/order_book?currency_pair={symbol}&limit=100&with_id=true"
          )
        }
        MarketType::Future => {
          format!(
            "https://fx-api.gateio.ws/api/v4/futures/usdt/order_book?contract={symbol}&limit=100&with_id=true"
          )
        }
      };

      let headers = from_headers!([("Accept", "application/json")]);

      let response = loop {
        match HTTP_LIMITER
          .get()
          .expect("Limiter not initialized")
          .try_wait()
        {
          Ok(()) => {
            break utils
              .http_client
              .request("GET".into(), uri, headers, None)
              .await?
              .body()
              .limit(10 * 1024 * 1024)
              .await?;
          }
          Err(duration) => {
            ntex::time::sleep(duration).await;
          }
        }
      };

      if let MarketType::Future = market {
        let result = serde_json::from_slice::<FutureDepthSnapshot>(&response)?;

        processed = OrderBook {
          asks: result
            .asks
            .into_iter()
            .map(|item| (item.p, item.s))
            .collect(),
          bids: result
            .bids
            .into_iter()
            .map(|item| (Reverse(item.p), item.s))
            .collect(),
          update_id: result.id,
        };
      } else {
        let result = serde_json::from_slice::<SpotDepthSnapshot>(&response)?;

        processed = OrderBook {
          asks: result.asks.into_iter().collect(),
          bids: result
            .bids
            .into_iter()
            .map(|(k, v)| (Reverse(k), v))
            .collect(),
          update_id: result.id,
        };
      }

      retries += 1;
    }

    Ok(processed)
  }
}