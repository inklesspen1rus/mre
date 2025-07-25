use std::{cmp::Reverse, collections::BTreeMap};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
  pub id: String,
  pub symbol: String,
  pub side: String,
  pub amount: Decimal,
  pub price: Decimal,
  pub status: String,
  pub timestamp: u64,
  pub filled: Decimal,
  pub remaning: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
  pub symbol: String,
  pub side: String,
  pub amount: Decimal,
  pub price: Decimal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[derive(Default)]
pub struct OrderBook {
  pub bids: BTreeMap<Reverse<Decimal>, Decimal>, // preço -> quantidade
  pub asks: BTreeMap<Decimal, Decimal>,          // preço -> quantidade
  pub update_id: u64,
}


#[derive(Clone, Debug)]
pub struct OrderBookUpdate {
  pub bids: Vec<(Decimal, Decimal)>,
  pub asks: Vec<(Decimal, Decimal)>,
  pub first_update_id: u64,
  pub last_update_id: u64,
  pub full: bool,
}

impl OrderBook {
  pub fn apply_update(&mut self, update: &OrderBookUpdate, updates: &mut Vec<String>) {
    if update.full {
      self.asks.clear();
      self.bids.clear();
    }

    let bids_len = update.bids.len();
    let asks_len = update.asks.len();

    let zero = Decimal::ZERO;

    for (price, qty) in update.bids.iter() {
      let key = Reverse(*price);
      if qty.eq(&zero) {
        let result = self.bids.remove(&key);
        if result.is_none() {
          updates.push(format!("Removal BID {} failed", price))
        }
      } else {
        self.bids.insert(key, *qty);
      }
    }

    for (price, qty) in update.asks.iter() {
      if qty.eq(&zero) {
        let result = self.asks.remove(price);
        if result.is_none() {
          updates.push(format!("Removal ASK {} failed", price))
        }
      } else {
        self.asks.insert(*price, *qty);
      }
    }

    if bids_len > 0 || asks_len > 0 {
      self.update_id += 1;
    }

    if update.last_update_id > 0 {
      self.update_id = update.last_update_id;
    }
  }

  // Opcional: retorna os bids/asks como vetores ordenados
  pub fn get_bids(&self) -> Vec<(Reverse<Decimal>, Decimal)> {
    self.bids.iter().map(|(&p, &q)| (p, q)).collect()
  }

  pub fn get_asks(&self) -> Vec<(Decimal, Decimal)> {
    self.asks.iter().map(|(&p, &q)| (p, q)).collect()
  }
}