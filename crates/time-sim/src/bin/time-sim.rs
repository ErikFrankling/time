//! Run a scripted day and keep the server up to look at.
//!
//!   time-sim serve crates/time-sim/scenarios/morning.toml
//!
//! Prints the server's address, then serves until killed, with the clock
//! frozen at the end of the scenario.

use std::path::Path;

use anyhow::{bail, Context};
use time_sim::world::World;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => {
            let path = args
                .get(1)
                .context("usage: time-sim serve <scenario.toml>")?;
            let mut w = World::load(Path::new(path))?;
            w.run()?;
            println!("{} ({} events)", w.base_url, w.events()?.len());
            loop {
                std::thread::park();
            }
        }
        _ => bail!("usage: time-sim serve <scenario.toml>"),
    }
}
