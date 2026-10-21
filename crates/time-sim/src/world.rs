//! The real server and the real agent, run against a scripted day.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use time_agent::hyprland::Hyprland;
use time_agent::probe::{match_tab, Audio, Input, InputSnapshot, Tab, Tabs, Window};
use time_agent::sender::{Delivery, HttpSender, Sender};
use time_agent::{Agent, AgentConfig};
use time_core::clock::{Clock, ManualClock, Millis, MINUTE_MS};
use time_core::event::Envelope;
use time_server::config::{DeviceConfig, DeviceKind};
use time_server::{AppState, Config, Store};

use crate::fake_hyprland::FakeHyprland;
use crate::phone::{self, PhoneSim};
use crate::scenario::{DesktopSegment, Resolved};

/// What one desktop is doing this minute, shared between the world and the
/// simulated probes.
type Now = Arc<Mutex<Option<DesktopSegment>>>;

struct SimInput {
    now: Now,
    clock: ManualClock,
    last_active: Option<Millis>,
}

impl Input for SimInput {
    fn take(&mut self) -> InputSnapshot {
        let seg = self.now.lock().unwrap().clone();
        let (keys, mouse) = seg.map(|s| (s.keys, s.mouse)).unwrap_or((0, 0));
        // The agent asks just after the minute ends.
        let minute_end = self.clock.now_ms() - self.clock.now_ms().rem_euclid(MINUTE_MS);
        if keys + mouse > 0 {
            self.last_active = Some(minute_end);
        }
        let idle_seconds = Some(match self.last_active {
            Some(t) => ((minute_end - t) / 1000) as u32,
            None => 3600,
        });
        InputSnapshot {
            keys,
            mouse,
            idle_seconds,
        }
    }
}

struct SimAudio(Now);

impl Audio for SimAudio {
    fn playing(&mut self) -> bool {
        self.0.lock().unwrap().as_ref().is_some_and(|s| s.audio)
    }
}

struct SimTabs(Now);

impl Tabs for SimTabs {
    fn front_tab(&mut self, focused: &Window) -> Option<Tab> {
        let seg = self.0.lock().unwrap().clone()?;
        let tabs: Vec<Tab> = seg
            .tabs
            .iter()
            .map(|t| Tab {
                host: t.host.clone(),
                title: t.title.clone(),
            })
            .collect();
        match_tab(focused, &tabs)
    }
}

/// A sender that cannot reach the server while the scenario says the device
/// is offline.
struct Flaky {
    inner: HttpSender,
    scenario: Arc<Resolved>,
    device: String,
    clock: ManualClock,
}

impl Sender for Flaky {
    fn send(&mut self, batch: &[Envelope]) -> Delivery {
        if self.scenario.offline(&self.device, self.clock.now_ms() - 1) {
            return Delivery::Retry("simulated: offline".into());
        }
        self.inner.send(batch)
    }
}

struct Desktop {
    agent: Agent,
    now: Now,
    hypr: FakeHyprland,
}

pub struct World {
    pub scenario: Arc<Resolved>,
    pub clock: ManualClock,
    pub base_url: String,
    pub state: Arc<AppState>,
    pub phone: Option<PhoneSim>,
    desktops: BTreeMap<String, Desktop>,
    _runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

impl World {
    pub fn start(scenario: Resolved) -> Result<Self> {
        let scenario = Arc::new(scenario);
        let dir = tempfile::tempdir()?;
        let clock = ManualClock::new(scenario.start_ms);

        let mut devices = BTreeMap::new();
        for d in &scenario.raw.desktops {
            devices.insert(
                d.clone(),
                DeviceConfig {
                    kind: DeviceKind::Desktop,
                },
            );
        }
        if scenario.raw.phone_enabled {
            devices.insert(
                phone::DEVICE.into(),
                DeviceConfig {
                    kind: DeviceKind::Phone,
                },
            );
        }
        let config = Config {
            data_dir: dir.path().join("server"),
            listen: "127.0.0.1:0".into(),
            devices,
            channels_off: vec![],
            rules: None,
            classify_every_secs: 10,
            timezone: "Europe/Stockholm".into(),
        };
        std::fs::create_dir_all(&config.data_dir)?;
        let store = Store::open(&config.data_dir.join("time.db"))?;
        let state = AppState::new(config, store, Arc::new(clock.clone()))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))?;
        let base_url = format!("http://{}", listener.local_addr()?);
        runtime.spawn(time_server::serve(listener, state.clone()));
        let endpoint = format!("{base_url}/v1/logs");

        let mut desktops = BTreeMap::new();
        for device in &scenario.raw.desktops {
            let hypr = FakeHyprland::start(&dir.path().join("hypr").join(device))?;
            let now: Now = Arc::new(Mutex::new(None));
            let config = AgentConfig::parse(&format!(
                "device = {device:?}\nendpoint = {endpoint:?}\nspool_dir = {:?}\n",
                dir.path().join("spool").join(device)
            ))?;
            let sender = Flaky {
                inner: HttpSender::new(&endpoint, None)?,
                scenario: scenario.clone(),
                device: device.clone(),
                clock: clock.clone(),
            };
            let agent = Agent::new(
                config,
                Arc::new(clock.clone()),
                Box::new(Hyprland::new(hypr.socket.clone())),
                Box::new(SimInput {
                    now: now.clone(),
                    clock: clock.clone(),
                    last_active: None,
                }),
                Box::new(SimAudio(now.clone())),
                Box::new(SimTabs(now.clone())),
                Box::new(sender),
            )?;
            desktops.insert(device.clone(), Desktop { agent, now, hypr });
        }

        let phone = scenario.raw.phone_enabled.then(|| {
            let sender = Flaky {
                inner: HttpSender::new(&endpoint, None).expect("http client"),
                scenario: scenario.clone(),
                device: phone::DEVICE.into(),
                clock: clock.clone(),
            };
            PhoneSim::new(Box::new(sender))
        });

        Ok(Self {
            scenario,
            clock,
            base_url,
            state,
            phone,
            desktops,
            _runtime: runtime,
            _dir: dir,
        })
    }

    pub fn load(path: &std::path::Path) -> Result<Self> {
        Self::start(crate::scenario::Scenario::load(path)?)
    }

    /// Run every minute of the scenario, then let every device deliver what
    /// it still holds, as it would once back online.
    pub fn run(&mut self) -> Result<()> {
        let minutes: Vec<Millis> = self.scenario.minutes().collect();
        for t in minutes {
            self.step(t)?;
        }
        self.finish()?;
        self.classify()?;
        Ok(())
    }

    /// Run the server's classifier until nothing is queued, as its
    /// background loop would.
    pub fn classify(&self) -> Result<usize> {
        let mut total = 0;
        loop {
            let n = self
                .state
                .classifier
                .run_once(&self.state.store, self.clock.now_ms())?;
            total += n;
            if n < time_server::classify::PASS_LIMIT {
                return Ok(total);
            }
        }
    }

    /// The labels the server holds for `[from, to)`.
    pub fn labels(&self, from: Millis, to: Millis) -> Result<Vec<time_core::label::MinuteLabel>> {
        self.state.store.labels_between(from, to)
    }

    /// Simulate the minute starting at `t`.
    pub fn step(&mut self, t: Millis) -> Result<()> {
        for (device, d) in &mut self.desktops {
            let seg = self.scenario.desktop_at(device, t).cloned();
            *d.hypr.focus.lock().unwrap() = seg.as_ref().and_then(|s| {
                s.class.as_ref().map(|c| Window {
                    class: c.clone(),
                    title: s.title.clone(),
                    fullscreen: s.fullscreen,
                })
            });
            *d.now.lock().unwrap() = seg;
        }
        // The agent ticks half a second after the minute ends.
        self.clock.set(t + MINUTE_MS + 500);
        for (device, d) in &mut self.desktops {
            d.agent
                .tick()
                .with_context(|| format!("{device} agent tick at {t}"))?;
        }
        if let Some(p) = &mut self.phone {
            p.minute(&self.scenario, t);
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        // Drain at the end of the last minute: extra ticks inside the same
        // minute record nothing new, they only send what is still spooled.
        let end = self.scenario.end_ms;
        self.clock.set(end + 500);
        for (device, d) in &mut self.desktops {
            anyhow::ensure!(
                !self.scenario.offline(device, end - 1),
                "{device} is offline at the end of the scenario"
            );
            for _ in 0..3 {
                d.agent.tick()?;
            }
        }
        if let Some(p) = &mut self.phone {
            p.finish(end);
        }
        Ok(())
    }

    pub fn now(&self) -> Millis {
        self.clock.now_ms()
    }

    /// GET a path on the server.
    pub fn get(&self, path: &str) -> Result<(u16, String)> {
        let resp = reqwest::blocking::get(format!("{}{path}", self.base_url))?;
        Ok((resp.status().as_u16(), resp.text()?))
    }

    pub fn status(&self) -> Result<serde_json::Value> {
        let (code, body) = self.get("/api/status")?;
        anyhow::ensure!(code == 200, "status {code}: {body}");
        Ok(serde_json::from_str(&body)?)
    }

    /// Every event the server stored, oldest first.
    pub fn events(&self) -> Result<Vec<Envelope>> {
        self.state.store.events_between(i64::MIN / 2, i64::MAX / 2)
    }
}
