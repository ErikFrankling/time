use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use time_core::clock::SystemClock;
use time_server::config::DeviceKind;
use time_server::{classify, import_v1, AppState, Config, Store};

const USAGE: &str = "usage:
  time-server serve <config.toml>             run the server
  time-server import-v1 <config.toml> <v1.db> turn v1's history into events
  time-server classify <config.toml>          label everything queued, then exit
  time-server compare-v1 <config.toml> <v1.db> compare v2's labels with v1's";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let config_path = args.get(1).map(PathBuf::from);
    match (cmd, config_path) {
        ("serve", Some(p)) => serve(&p).await,
        ("import-v1", Some(p)) => {
            let v1 = args.get(2).context(USAGE)?;
            let (config, state) = open(&p)?;
            let desktops = devices(&config, |k| k != DeviceKind::Phone);
            let phones = devices(&config, |k| k == DeviceKind::Phone);
            let r = import_v1::import(
                Path::new(v1),
                &state.store,
                &desktops,
                &phones,
                state.clock.now_ms(),
            )?;
            println!("{r:#?}");
            Ok(())
        }
        ("classify", Some(p)) => {
            let (_, state) = open(&p)?;
            let started = std::time::Instant::now();
            let mut total = 0;
            loop {
                let n = state
                    .classifier
                    .run_once(&state.store, state.clock.now_ms())?;
                total += n;
                if n < classify::PASS_LIMIT {
                    break;
                }
            }
            println!("labelled {total} minute(s) in {:.1?}", started.elapsed());
            Ok(())
        }
        ("compare-v1", Some(p)) => {
            let v1 = args.get(2).context(USAGE)?;
            let (_, state) = open(&p)?;
            let c = import_v1::compare(Path::new(v1), &state.store)?;
            let pct = |n: usize, d: usize| {
                if d == 0 {
                    0.0
                } else {
                    100.0 * n as f64 / d as f64
                }
            };
            println!("v1 model-labelled minutes where v2 also put Erik on that device:");
            println!(
                "  v2 decided:   {}  (agree with v1: {:.1}%)",
                c.compared,
                pct(c.agree, c.compared)
            );
            println!(
                "  v2 unknown:   {}  ({:.1}% of those minutes)",
                c.unknown,
                pct(c.unknown, c.unknown + c.compared)
            );
            println!("top disagreements (v1 -> v2):");
            let mut conf: Vec<_> = c.confusions.into_iter().collect();
            conf.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            for ((a, b), n) in conf.into_iter().take(15) {
                println!("  {a:>9} -> {b:<9} {n}");
            }
            println!("top unknown windows:");
            for (w, n) in c.top_unknown {
                println!("  {n:>5}  {}", w.chars().take(100).collect::<String>());
            }
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

fn devices(config: &Config, pick: impl Fn(DeviceKind) -> bool) -> Vec<String> {
    config
        .devices
        .iter()
        .filter(|(_, d)| pick(d.kind))
        .map(|(n, _)| n.clone())
        .collect()
}

fn open(path: &Path) -> anyhow::Result<(Config, Arc<AppState>)> {
    let config = Config::load(path)?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating {}", config.data_dir.display()))?;
    let store = Store::open(&config.data_dir.join("time.db"))?;
    let state = AppState::new(config.clone(), store, Arc::new(SystemClock))?;
    Ok((config, state))
}

async fn serve(path: &Path) -> anyhow::Result<()> {
    let (config, state) = open(path)?;
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    eprintln!(
        "time-server {} listening on {}, rules {} ({} rules)",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr()?,
        state.classifier.version(),
        state.classifier.current().rules.rules.len()
    );
    tokio::spawn(classify::run_forever(state.clone()));
    time_server::apk::spawn_poller(state.apk.clone());
    time_server::serve(listener, state).await
}
