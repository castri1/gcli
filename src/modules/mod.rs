//! Feature modules, one per screen. To add one (Costs, Builds, Logs…):
//!   1. add the Google Cloud calls in `src/gcp/<service>.rs` (typed structs + `gcp.get`/`pages`),
//!   2. implement `tui::module::Module` in `src/modules/<name>.rs`,
//!   3. register it below. The header and the `:` palette list modules in this order.

mod apps;
mod builds;
mod common;
mod costs;
mod crons;
mod db;
mod logs;
mod platform;

use crate::tui::module::Module;

pub fn all() -> Vec<Box<dyn Module>> {
    vec![
        Box::new(apps::Apps::new()),
        Box::new(costs::Costs::new()),
        Box::new(builds::Builds::new()),
        Box::new(logs::Logs::new()),
        Box::new(db::Database::new()),
        Box::new(crons::Crons::new()),
        Box::new(platform::Platform::new()),
    ]
}
