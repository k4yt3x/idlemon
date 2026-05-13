use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures_util::StreamExt as _;
use tokio::sync::Mutex;
use x11rb::protocol::screensaver::ConnectionExt as _;
use x11rb::protocol::xproto::Window;
use x11rb::rust_connection::RustConnection;
use zbus::fdo::NameOwnerChangedStream;
use zbus::message::Header;
use zbus::names::UniqueName;
use zbus::object_server::{InterfaceRef, SignalEmitter};

pub const WELL_KNOWN_NAME: &str = "org.gnome.Mutter.IdleMonitor";
pub const OBJECT_PATH: &str = "/org/gnome/Mutter/IdleMonitor/Core";
const POLL_INTERVAL: Duration = Duration::from_secs(1);
// The internal "is the user idle?" threshold is derived from the poll interval
// because at the polling cadence we cannot resolve idle/active transitions any
// finer. A smaller value would cause spurious active-watch fires whenever the
// X11 idle counter happens to be sampled mid-poll during continuous activity.
const INTERNAL_IDLE_THRESHOLD_MS: u64 = POLL_INTERVAL.as_millis() as u64;

pub struct IdleMonitor {
    x11_connection: Arc<RustConnection>,
    root_window: Window,
    state: Arc<Mutex<State>>,
}

pub struct IdleWatch {
    interval_ms: u64,
    fired_this_cycle: bool,
}

pub struct State {
    next_watch_id: u32,
    last_idle_ms: u64,
    was_idle: bool,
    idle_watches: HashMap<u32, IdleWatch>,
    active_watches: HashSet<u32>,
    watch_owners: HashMap<u32, UniqueName<'static>>,
}

impl State {
    fn new() -> Self {
        Self {
            next_watch_id: 1,
            last_idle_ms: 0,
            was_idle: false,
            idle_watches: HashMap::new(),
            active_watches: HashSet::new(),
            watch_owners: HashMap::new(),
        }
    }

    fn allocate_watch_id(&mut self) -> u32 {
        loop {
            let candidate = self.next_watch_id;
            self.next_watch_id = self.next_watch_id.wrapping_add(1);
            if candidate == 0 {
                continue;
            }
            if !self.idle_watches.contains_key(&candidate)
                && !self.active_watches.contains(&candidate)
                && !self.watch_owners.contains_key(&candidate)
            {
                return candidate;
            }
        }
    }

    pub fn evaluate_tick(&mut self, current_idle_ms: u64) -> Vec<u32> {
        let mut fired = Vec::new();
        let is_idle_now = current_idle_ms >= INTERNAL_IDLE_THRESHOLD_MS;
        let counter_reset = current_idle_ms < self.last_idle_ms;

        // The X11 idle counter only drops when the server observes input, so a
        // decrease re-arms every idle watch. Re-arming here (not on threshold
        // transitions alone) ensures watches with intervals below the internal
        // threshold can fire again on subsequent idle cycles.
        if counter_reset {
            for watch in self.idle_watches.values_mut() {
                watch.fired_this_cycle = false;
            }
        }

        // Active watches fire on the user-active transition against the
        // internal threshold, independent of which idle watches are registered.
        if self.was_idle && !is_idle_now {
            for watch_id in self.active_watches.drain() {
                self.watch_owners.remove(&watch_id);
                fired.push(watch_id);
            }
        }

        for (watch_id, watch) in self.idle_watches.iter_mut() {
            if !watch.fired_this_cycle && watch.interval_ms <= current_idle_ms {
                watch.fired_this_cycle = true;
                fired.push(*watch_id);
            }
        }

        self.was_idle = is_idle_now;
        self.last_idle_ms = current_idle_ms;
        fired
    }

    pub fn remove_owner(&mut self, owner: &UniqueName<'_>) -> Vec<u32> {
        let mut removed = Vec::new();
        let owner_str = owner.as_str();
        self.watch_owners.retain(|watch_id, watch_owner| {
            if watch_owner.as_str() == owner_str {
                removed.push(*watch_id);
                false
            } else {
                true
            }
        });
        for watch_id in &removed {
            self.idle_watches.remove(watch_id);
            self.active_watches.remove(watch_id);
        }
        removed
    }
}

impl IdleMonitor {
    pub fn new(
        x11_connection: Arc<RustConnection>,
        root_window: Window,
    ) -> (Self, Arc<Mutex<State>>) {
        let state = Arc::new(Mutex::new(State::new()));
        let monitor = Self {
            x11_connection,
            root_window,
            state: state.clone(),
        };
        (monitor, state)
    }
}

#[zbus::interface(name = "org.gnome.Mutter.IdleMonitor")]
impl IdleMonitor {
    #[zbus(name = "GetIdletime")]
    async fn get_idletime(&self) -> zbus::fdo::Result<u64> {
        query_idle_ms(self.x11_connection.clone(), self.root_window)
            .await
            .map_err(|error| {
                zbus::fdo::Error::Failed(format!("X11 idle-time query failed: {error}"))
            })
    }

    #[zbus(name = "AddIdleWatch")]
    async fn add_idle_watch(
        &self,
        #[zbus(header)] header: Header<'_>,
        interval_ms: u64,
    ) -> zbus::fdo::Result<u32> {
        let sender = sender_from_header(&header)?;
        let mut state = self.state.lock().await;
        let watch_id = state.allocate_watch_id();
        state.idle_watches.insert(
            watch_id,
            IdleWatch {
                interval_ms,
                fired_this_cycle: false,
            },
        );
        state.watch_owners.insert(watch_id, sender);
        tracing::debug!(watch_id, interval_ms, "registered idle watch");
        Ok(watch_id)
    }

    #[zbus(name = "AddUserActiveWatch")]
    async fn add_user_active_watch(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<u32> {
        let sender = sender_from_header(&header)?;
        let mut state = self.state.lock().await;
        let watch_id = state.allocate_watch_id();
        state.active_watches.insert(watch_id);
        state.watch_owners.insert(watch_id, sender);
        tracing::debug!(watch_id, "registered user-active watch");
        Ok(watch_id)
    }

    #[zbus(name = "RemoveWatch")]
    async fn remove_watch(&self, watch_id: u32) -> zbus::fdo::Result<()> {
        let mut state = self.state.lock().await;
        state.idle_watches.remove(&watch_id);
        state.active_watches.remove(&watch_id);
        state.watch_owners.remove(&watch_id);
        tracing::debug!(watch_id, "removed watch");
        Ok(())
    }

    #[zbus(signal, name = "WatchFired")]
    async fn watch_fired(signal_emitter: &SignalEmitter<'_>, watch_id: u32) -> zbus::Result<()>;
}

fn sender_from_header(header: &Header<'_>) -> zbus::fdo::Result<UniqueName<'static>> {
    header
        .sender()
        .ok_or_else(|| zbus::fdo::Error::Failed("D-Bus message has no sender".to_string()))
        .map(|sender| sender.to_owned())
}

pub async fn run_poll_loop(
    interface_ref: InterfaceRef<IdleMonitor>,
    state: Arc<Mutex<State>>,
    x11_connection: Arc<RustConnection>,
    root_window: Window,
) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Drop the initial immediate tick so the first evaluation happens after one full interval,
    // giving clients a moment to register watches.
    ticker.tick().await;

    loop {
        ticker.tick().await;

        let current_idle_ms = query_idle_ms(x11_connection.clone(), root_window)
            .await
            .context("X11 idle-time query")?;

        let to_fire: Vec<u32> = {
            let mut state = state.lock().await;
            state.evaluate_tick(current_idle_ms)
        };

        for watch_id in to_fire {
            if let Err(error) =
                IdleMonitor::watch_fired(interface_ref.signal_emitter(), watch_id).await
            {
                tracing::warn!(watch_id, %error, "failed to emit WatchFired signal");
            } else {
                tracing::debug!(watch_id, "fired watch");
            }
        }
    }
}

pub async fn run_cleanup_loop(
    state: Arc<Mutex<State>>,
    mut stream: NameOwnerChangedStream,
) -> anyhow::Result<()> {
    while let Some(signal) = stream.next().await {
        let args = match signal.args() {
            Ok(args) => args,
            Err(error) => {
                tracing::warn!(%error, "failed to decode NameOwnerChanged signal");
                continue;
            }
        };
        // Only act on disappearances (new_owner empty). well-known name changes
        // are uninteresting because we keyed ownership by unique connection name.
        if args.new_owner.is_some() {
            continue;
        }
        let Some(old_owner) = args.old_owner.as_ref() else {
            continue;
        };
        let removed = {
            let mut state = state.lock().await;
            state.remove_owner(old_owner)
        };
        if !removed.is_empty() {
            tracing::debug!(owner = %old_owner.as_str(), ?removed, "removed watches for vanished sender");
        }
    }
    // Stream end means the session bus is no longer delivering NameOwnerChanged,
    // so we can no longer reap watches from disconnected clients. Treat this as
    // fatal so the supervisor sees a real exit rather than a silent half-broken
    // daemon.
    anyhow::bail!("NameOwnerChanged stream ended; owner cleanup is no longer possible")
}

async fn query_idle_ms(
    x11_connection: Arc<RustConnection>,
    root_window: Window,
) -> anyhow::Result<u64> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let reply = x11_connection
            .screensaver_query_info(root_window)?
            .reply()?;
        Ok(u64::from(reply.ms_since_user_input))
    })
    .await
    .context("X11 query task join")?
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    fn unique(name: &str) -> UniqueName<'static> {
        UniqueName::try_from(name.to_string()).expect("valid unique name")
    }

    fn idle_watch(interval_ms: u64) -> IdleWatch {
        IdleWatch {
            interval_ms,
            fired_this_cycle: false,
        }
    }

    #[test]
    fn allocate_watch_id_skips_zero_after_wrap() {
        let mut state = State::new();
        state.next_watch_id = u32::MAX;
        let first = state.allocate_watch_id();
        assert_eq!(first, u32::MAX);
        let second = state.allocate_watch_id();
        assert_eq!(second, 1);
    }

    #[test]
    fn allocate_watch_id_skips_existing_ids() {
        let mut state = State::new();
        state.idle_watches.insert(1, idle_watch(5_000));
        state.active_watches.insert(2);
        let id = state.allocate_watch_id();
        assert_eq!(id, 3);
    }

    #[test]
    fn idle_watch_refires_across_cycles() {
        let mut state = State::new();
        state.idle_watches.insert(1, idle_watch(5_000));
        state.watch_owners.insert(1, unique(":1.5"));

        assert!(state.evaluate_tick(3_000).is_empty());
        assert!(state.idle_watches.contains_key(&1));

        let fired = state.evaluate_tick(6_000);
        assert_eq!(fired, vec![1]);
        assert!(state.idle_watches.contains_key(&1));
        assert!(state.watch_owners.contains_key(&1));

        assert!(state.evaluate_tick(7_000).is_empty());

        assert!(state.evaluate_tick(100).is_empty());
        assert!(state.idle_watches.contains_key(&1));

        let fired = state.evaluate_tick(5_500);
        assert_eq!(fired, vec![1]);
        assert!(state.idle_watches.contains_key(&1));
        assert!(state.watch_owners.contains_key(&1));
    }

    #[test]
    fn active_watch_fires_without_idle_watch_registered() {
        let mut state = State::new();
        state.active_watches.insert(1);
        state.watch_owners.insert(1, unique(":1.5"));

        assert!(state.evaluate_tick(1_500).is_empty());
        assert!(state.was_idle);
        assert!(state.evaluate_tick(2_500).is_empty());

        let fired = state.evaluate_tick(200);
        assert_eq!(fired, vec![1]);
        assert!(!state.was_idle);
        assert!(state.active_watches.is_empty());
        assert!(!state.watch_owners.contains_key(&1));
    }

    #[test]
    fn active_watch_does_not_fire_on_subsecond_jitter() {
        let mut state = State::new();
        state.active_watches.insert(1);
        state.watch_owners.insert(1, unique(":1.5"));

        for current_idle_ms in [500u64, 800, 200, 700, 300] {
            assert!(state.evaluate_tick(current_idle_ms).is_empty());
            assert!(!state.was_idle);
        }
        assert!(state.active_watches.contains(&1));
        assert!(state.watch_owners.contains_key(&1));
    }

    #[test]
    fn idle_watch_with_subthreshold_interval_rearms_on_input() {
        let mut state = State::new();
        state.idle_watches.insert(1, idle_watch(200));
        state.watch_owners.insert(1, unique(":1.5"));

        let fired = state.evaluate_tick(300);
        assert_eq!(fired, vec![1]);
        assert!(state.idle_watches.contains_key(&1));

        assert!(state.evaluate_tick(50).is_empty());

        let fired = state.evaluate_tick(300);
        assert_eq!(fired, vec![1]);
    }

    #[test]
    fn multiple_idle_watches_fire_in_same_cycle() {
        let mut state = State::new();
        state.idle_watches.insert(1, idle_watch(1_000));
        state.idle_watches.insert(2, idle_watch(5_000));
        state.idle_watches.insert(3, idle_watch(10_000));
        state.watch_owners.insert(1, unique(":1.5"));
        state.watch_owners.insert(2, unique(":1.5"));
        state.watch_owners.insert(3, unique(":1.5"));

        let fired = state.evaluate_tick(1_100);
        assert_eq!(fired, vec![1]);

        let fired = state.evaluate_tick(5_100);
        assert_eq!(fired, vec![2]);

        let fired = state.evaluate_tick(10_100);
        assert_eq!(fired, vec![3]);

        assert!(state.evaluate_tick(50).is_empty());

        let fired = state.evaluate_tick(1_100);
        assert_eq!(fired, vec![1]);
    }

    #[test]
    fn active_watch_fires_after_idle_then_resume() {
        let mut state = State::new();
        state.idle_watches.insert(1, idle_watch(5_000));
        state.active_watches.insert(2);
        state.watch_owners.insert(1, unique(":1.5"));
        state.watch_owners.insert(2, unique(":1.6"));

        let fired = state.evaluate_tick(6_000);
        assert_eq!(fired, vec![1]);
        assert!(state.was_idle);
        assert!(state.idle_watches.contains_key(&1));

        let fired = state.evaluate_tick(100);
        assert_eq!(fired, vec![2]);
        assert!(!state.was_idle);
        assert!(state.active_watches.is_empty());
        assert!(!state.watch_owners.contains_key(&2));
        assert!(state.idle_watches.contains_key(&1));
        assert!(state.watch_owners.contains_key(&1));
    }

    #[test]
    fn idle_watch_registered_while_already_idle_fires_next_tick() {
        let mut state = State::new();
        state.was_idle = true;
        state.last_idle_ms = 8_000;
        state.idle_watches.insert(1, idle_watch(5_000));
        state.watch_owners.insert(1, unique(":1.5"));

        let fired = state.evaluate_tick(9_000);
        assert_eq!(fired, vec![1]);
        assert!(state.idle_watches.contains_key(&1));
    }

    #[test]
    fn watch_owners_persist_after_idle_watch_fires() {
        let mut state = State::new();
        let owner = unique(":1.5");
        state.idle_watches.insert(1, idle_watch(5_000));
        state.watch_owners.insert(1, owner.clone());

        let fired = state.evaluate_tick(6_000);
        assert_eq!(fired, vec![1]);
        assert!(state.watch_owners.contains_key(&1));

        let removed = state.remove_owner(&owner);
        assert_eq!(removed, vec![1]);
        assert!(!state.idle_watches.contains_key(&1));
        assert!(!state.watch_owners.contains_key(&1));
    }

    #[test]
    fn remove_owner_drops_only_matching_watches() {
        let mut state = State::new();
        let alice = unique(":1.5");
        let bob = unique(":1.6");
        state.idle_watches.insert(1, idle_watch(5_000));
        state.idle_watches.insert(2, idle_watch(10_000));
        state.active_watches.insert(3);
        state.watch_owners.insert(1, alice.clone());
        state.watch_owners.insert(2, bob.clone());
        state.watch_owners.insert(3, alice.clone());

        let mut removed = state.remove_owner(&alice);
        removed.sort();
        assert_eq!(removed, vec![1, 3]);
        assert!(!state.idle_watches.contains_key(&1));
        assert!(state.idle_watches.contains_key(&2));
        assert!(!state.active_watches.contains(&3));
        assert!(!state.watch_owners.contains_key(&1));
        assert!(state.watch_owners.contains_key(&2));
        assert!(!state.watch_owners.contains_key(&3));
    }
}
