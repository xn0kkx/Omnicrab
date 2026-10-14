pub mod app;
pub mod crab;
pub mod form;
pub mod input;
pub mod ui;

use anyhow::Result;

use crate::config::Config;
use crate::store::Store;
use crate::usage::PriceTable;

pub async fn run(store: Store, cfg: &Config) -> Result<()> {
    let (app, rx, lane_rx) = app::App::new(store, PriceTable::new(&cfg.prices)).await?;
    app.run(rx, lane_rx).await
}
