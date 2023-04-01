use barter::{
    data::historical,
    engine::{trader::Trader, Engine},
    event::{Event, EventTx},
    execution::{
        simulated::{Config as ExecutionConfig, SimulatedExecution},
        Fees,
    },
    portfolio::{
        allocator::DefaultAllocator, portfolio::MetaPortfolio,
        repository::in_memory::InMemoryRepository, risk::DefaultRisk,
    },
    statistic::summary::{
        trading::{Config as StatisticConfig, TradingSummary},
        Initialiser,
    },
    strategy::example::{Config as StrategyConfig, RSIStrategy},
};
use barter_data::subscription::candle::Candle;
use barter_data::{
    event::{DataKind, MarketEvent},
    ExchangeWsStream,
};
use barter_integration::model::{Exchange, Instrument, InstrumentKind, Market};
use chrono::{DateTime, TimeZone, Utc};
use csv;
use parking_lot::Mutex;
use serde::Deserialize;
use std::{collections::HashMap, fs, sync::Arc};
use tokio::sync::mpsc;
use uuid::Uuid;

const DATA_HISTORIC_CANDLES_1H: &str = "examples/data/candles_1h.json";
const DATA_CSV_HISTORIC_CANDLES_1H: &str = "examples/data/BTCUSDT-5m-2022-01.csv";

#[tokio::main]
async fn main() {
    let data = load_csv_market_event_candles();
    // Create channel to distribute Commands to the Engine & it's Traders (eg/ Command::Terminate)
    let (_command_tx, command_rx) = mpsc::channel(20);

    // Create Event channel to listen to all Engine Events in real-time
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let event_tx = EventTx::new(event_tx);

    // Generate unique identifier to associate an Engine's components
    let engine_id = Uuid::new_v4();

    // Create the Market(s) to be traded on (1-to-1 relationship with a Trader)
    let market = Market::new("binance", ("btc", "usdt", InstrumentKind::Spot));

    // Build global shared-state MetaPortfolio (1-to-1 relationship with an Engine)
    let portfolio = Arc::new(Mutex::new(
        MetaPortfolio::builder()
            .engine_id(engine_id)
            .markets(vec![market.clone()])
            .starting_cash(10_000.0)
            .repository(InMemoryRepository::new())
            .allocation_manager(DefaultAllocator {
                default_order_value: 100.0,
            })
            .risk_manager(DefaultRisk {})
            .statistic_config(StatisticConfig {
                starting_equity: 10_000.0,
                trading_days_per_year: 365,
                risk_free_return: 0.0,
            })
            .build_and_init()
            .expect("failed to build & initialise MetaPortfolio"),
    ));

    // Build Trader(s)
    let mut traders = Vec::new();

    // Create channel for each Trader so the Engine can distribute Commands to it
    let (trader_command_tx, trader_command_rx) = mpsc::channel(10);

    traders.push(
        Trader::builder()
            .engine_id(engine_id)
            .market(market.clone())
            .command_rx(trader_command_rx)
            .event_tx(event_tx.clone())
            .portfolio(Arc::clone(&portfolio))
            .data(historical::MarketFeed::new(
                data,
            ))
            .strategy(RSIStrategy::new(StrategyConfig { rsi_period: 14 }))
            .execution(SimulatedExecution::new(ExecutionConfig {
                simulated_fees_pct: Fees {
                    exchange: 0.1,
                    slippage: 0.05,
                    network: 0.0,
                },
            }))
            .build()
            .expect("failed to build trader"),
    );

    // Build Engine (1-to-many relationship with Traders)
    // Create HashMap<Market, trader_command_tx> so Engine can route Commands to Traders
    let trader_command_txs = HashMap::from([(market, trader_command_tx)]);

    let engine = Engine::builder()
        .engine_id(engine_id)
        .command_rx(command_rx)
        .portfolio(portfolio)
        .traders(traders)
        .trader_command_txs(trader_command_txs)
        .statistics_summary(TradingSummary::init(StatisticConfig {
            starting_equity: 1000.0,
            trading_days_per_year: 365,
            risk_free_return: 0.0,
        }))
        .build()
        .expect("failed to build engine");

    // Run Engine trading & listen to Events it produces
    tokio::spawn(listen_to_engine_events(event_rx));
    engine.run().await;
}

fn load_json_market_event_candles() -> Vec<MarketEvent<DataKind>> {
    let candles = fs::read_to_string(DATA_HISTORIC_CANDLES_1H).expect("failed to read file");

    let candles =
        serde_json::from_str::<Vec<Candle>>(&candles).expect("failed to parse candles String");

    candles
        .into_iter()
        .map(|candle| MarketEvent {
            exchange_time: candle.close_time,
            received_time: Utc::now(),
            exchange: Exchange::from("binance"),
            instrument: Instrument::from(("btc", "usdt", InstrumentKind::Spot)),
            kind: DataKind::Candle(candle),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct BinanceCandle {
    open_time: u64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    close_time: u64,
    quote_asset_volume: f64,
    number_of_trades: u64,
    taker_buy_base_asset_volume: f64,
    taker_buy_quote_asset_volume: f64,
    ignore: f64,
}

impl Into<Candle> for BinanceCandle {
    fn into(self) -> Candle {
        let close_time = DateTime::<Utc>::from_utc(
            chrono::NaiveDateTime::from_timestamp_opt(self.close_time as i64 / 1000, 0).unwrap(),
            Utc,
        );

        Candle {
            close_time,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            trade_count: self.number_of_trades,
        }
    }
}

fn load_csv_market_event_candles() -> Vec<MarketEvent<DataKind>> {
    let mut candles = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_path(DATA_CSV_HISTORIC_CANDLES_1H)
        .expect("fuck csv");

    let mut ret = Vec::new();
    for result in candles.deserialize() {
        let record: BinanceCandle = result.unwrap();
        println!("{:?}", record);
        ret.push(MarketEvent {
            exchange_time: Utc.timestamp_millis_opt(record.close_time as i64).unwrap(),
            received_time: Utc::now(),
            exchange: Exchange::from("binance"),
            instrument: Instrument::from(("btc", "usdt", InstrumentKind::Spot)),
            kind: DataKind::Candle(record.into()),
        });
    }
    ret
}

// Listen to Events that occur in the Engine. These can be used for updating event-sourcing,
// updating dashboard, etc etc.
async fn listen_to_engine_events(mut event_rx: mpsc::UnboundedReceiver<Event>) {
    while let Some(event) = event_rx.recv().await {
        match event {
            Event::Market(_) => {
                // Market Event occurred in Engine
            }
            Event::Signal(signal) => {
                // Signal Event occurred in Engine
                println!("{signal:?}");
            }
            Event::SignalForceExit(_) => {
                // SignalForceExit Event occurred in Engine
            }
            Event::OrderNew(new_order) => {
                // OrderNew Event occurred in Engine
                println!("{new_order:?}");
            }
            Event::OrderUpdate => {
                // OrderUpdate Event occurred in Engine
            }
            Event::Fill(fill_event) => {
                // Fill Event occurred in Engine
                println!("{fill_event:?}");
            }
            Event::PositionNew(new_position) => {
                // PositionNew Event occurred in Engine
                println!("{new_position:?}");
            }
            Event::PositionUpdate(updated_position) => {
                // PositionUpdate Event occurred in Engine
                println!("{updated_position:?}");
            }
            Event::PositionExit(exited_position) => {
                // PositionExit Event occurred in Engine
                println!("{exited_position:?}");
            }
            Event::Balance(balance_update) => {
                // Balance update Event occurred in Engine
                println!("{balance_update:?}");
            }
        }
    }
}
