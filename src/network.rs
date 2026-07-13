// Network monitoring combines NetworkManager's event-driven topology state
// with adaptive ICMP reachability probes. Link type, active AP, and Wi-Fi
// strength never poll: PropertiesChanged wakes this task and it resnapshots the
// small set of relevant objects. Internet reachability is different because a
// working local link cannot announce an upstream outage, so randomized probes
// fill that gap without turning the UI into a fixed-interval polling loop.

use std::collections::HashSet;
use std::net::IpAddr;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::process::Command;
use tracing::{debug, error, info, warn};
use zbus::fdo;
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MatchRule, Proxy};

use crate::bus::Bus;

const NETWORK_MANAGER: &str = "org.freedesktop.NetworkManager";
const NETWORK_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager";
const NETWORK_MANAGER_IFACE: &str = "org.freedesktop.NetworkManager";
const ACTIVE_CONNECTION_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const WIRELESS_DEVICE_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const ACCESS_POINT_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";

const ICON_GLOBE: &str = "\u{f0ac}";
const ICON_ETHERNET: &str = "\u{f0200}";
const ICON_NETWORK: &str = "\u{f06f3}";
const ICON_NETWORK_OFF: &str = "\u{f0c9b}";
const ICON_WIFI_OUTLINE: &str = "\u{f092f}";
const ICON_WIFI_1: &str = "\u{f091f}";
const ICON_WIFI_2: &str = "\u{f0922}";
const ICON_WIFI_3: &str = "\u{f0925}";
const ICON_WIFI_4: &str = "\u{f0928}";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkConfig {
    pub ping_targets: Vec<IpAddr>,
    pub stable_mean: Duration,
    pub unstable_mean: Duration,
    pub outage_confirmation: Duration,
    pub recent_instability: Duration,
    pub ping_timeout: Duration,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            ping_targets: [
                "1.1.1.1",
                "1.0.0.1",
                "2606:4700:4700::1111",
                "2606:4700:4700::1001",
            ]
            .into_iter()
            .map(|target| target.parse().expect("hard-coded Cloudflare IP is valid"))
            .collect(),
            stable_mean: Duration::from_secs(60),
            unstable_mean: Duration::from_secs(1),
            outage_confirmation: Duration::from_secs(15),
            recent_instability: Duration::from_secs(60),
            ping_timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Link {
    None,
    Wifi { strength: u8 },
    Wired,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reachability {
    Unknown,
    Online,
    Offline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NetworkSnapshot {
    link: Link,
    primary_connection: Option<String>,
    device: Option<String>,
    access_point: Option<String>,
    nm_state: u32,
    nm_connectivity: u32,
}

impl NetworkSnapshot {
    fn disconnected() -> Self {
        Self {
            link: Link::None,
            primary_connection: None,
            device: None,
            access_point: None,
            nm_state: 20,
            nm_connectivity: 1,
        }
    }

    fn has_network(&self) -> bool {
        !matches!(self.link, Link::None)
    }

    fn same_connection(&self, other: &Self) -> bool {
        let same_kind = matches!(
            (&self.link, &other.link),
            (Link::None, Link::None)
                | (Link::Wifi { .. }, Link::Wifi { .. })
                | (Link::Wired, Link::Wired)
                | (Link::Other, Link::Other)
        );
        same_kind && self.primary_connection == other.primary_connection
    }

    fn event_path_is_relevant(&self, path: &str) -> bool {
        path == NETWORK_MANAGER_PATH
            || self.primary_connection.as_deref() == Some(path)
            || self.device.as_deref() == Some(path)
            || self.access_point.as_deref() == Some(path)
    }
}

fn display_text(link: &Link, reachability: Reachability) -> String {
    let reachability = match reachability {
        Reachability::Unknown => "?",
        Reachability::Online => ICON_GLOBE,
        Reachability::Offline => "×",
    };
    match link {
        Link::None => format!("{ICON_NETWORK_OFF} ×"),
        Link::Wifi { strength } => {
            let icon = match strength {
                0..=20 => ICON_WIFI_OUTLINE,
                21..=40 => ICON_WIFI_1,
                41..=60 => ICON_WIFI_2,
                61..=80 => ICON_WIFI_3,
                _ => ICON_WIFI_4,
            };
            format!("{icon} {strength}% {reachability}")
        }
        Link::Wired => format!("{ICON_ETHERNET} {reachability}"),
        Link::Other => format!("{ICON_NETWORK} {reachability}"),
    }
}

#[derive(Debug)]
struct ProbeResult {
    target: IpAddr,
    success: bool,
    elapsed: Duration,
}

#[derive(Debug)]
struct ProbeHealth {
    reachability: Reachability,
    first_failure: Option<Instant>,
    failed_targets: HashSet<IpAddr>,
    latency_ewma: Option<Duration>,
    unstable_until: Option<Instant>,
}

impl ProbeHealth {
    fn new() -> Self {
        Self {
            reachability: Reachability::Unknown,
            first_failure: None,
            failed_targets: HashSet::new(),
            latency_ewma: None,
            unstable_until: None,
        }
    }

    fn reset_for_no_network(&mut self) {
        self.reachability = Reachability::Offline;
        self.first_failure = None;
        self.failed_targets.clear();
        self.latency_ewma = None;
    }

    fn connection_changed(&mut self, now: Instant, config: &NetworkConfig) {
        self.reachability = Reachability::Unknown;
        self.first_failure = None;
        self.failed_targets.clear();
        self.latency_ewma = None;
        self.unstable_until = Some(now + config.recent_instability);
    }

    fn record(&mut self, result: &ProbeResult, now: Instant, config: &NetworkConfig) {
        if result.success {
            let had_failure = self.first_failure.take().is_some();
            self.failed_targets.clear();
            self.reachability = Reachability::Online;
            self.latency_ewma = Some(match self.latency_ewma {
                Some(previous) => weighted_duration(previous, result.elapsed, 4, 1),
                None => result.elapsed,
            });
            if had_failure {
                self.unstable_until = Some(now + config.recent_instability);
            }
            return;
        }

        self.first_failure.get_or_insert(now);
        self.failed_targets.insert(result.target);
        self.unstable_until = Some(now + config.recent_instability);

        let old_enough = self
            .first_failure
            .is_some_and(|first| now.duration_since(first) >= config.outage_confirmation);
        if old_enough && self.failed_targets.len() == config.ping_targets.len() {
            self.reachability = Reachability::Offline;
        }
    }

    fn expected_interval(&self, link: &Link, now: Instant, config: &NetworkConfig) -> Duration {
        if self.reachability == Reachability::Offline
            || self.first_failure.is_some()
            || self.unstable_until.is_some_and(|until| now < until)
        {
            return config.unstable_mean;
        }

        let mut mean = config.stable_mean;
        if let Link::Wifi { strength } = link {
            if *strength < 35 {
                mean = duration_div(mean, 10);
            } else if *strength < 60 {
                mean = duration_div(mean, 3);
            }
        }
        if let Some(latency) = self.latency_ewma {
            if latency >= Duration::from_millis(500) {
                mean = mean.min(duration_div(config.stable_mean, 10));
            } else if latency >= Duration::from_millis(150) {
                mean = mean.min(duration_div(config.stable_mean, 3));
            }
        }
        mean.max(config.unstable_mean)
    }
}

fn duration_div(duration: Duration, divisor: u32) -> Duration {
    duration.checked_div(divisor).unwrap_or(Duration::ZERO)
}

fn weighted_duration(
    previous: Duration,
    current: Duration,
    old_weight: u32,
    new_weight: u32,
) -> Duration {
    let total = old_weight + new_weight;
    duration_div(previous * old_weight + current * new_weight, total)
}

#[derive(Debug)]
struct ProbeRng(u64);

impl ProbeRng {
    fn seeded() -> Self {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let seed = time ^ u64::from(std::process::id()).rotate_left(17);
        Self(seed.max(1))
    }

    #[cfg(test)]
    fn from_seed(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn index(&mut self, upper_bound: usize) -> usize {
        (self.next_u64() % upper_bound as u64) as usize
    }

    fn jittered(&mut self, mean: Duration) -> Duration {
        let max_nanos = mean.as_nanos().saturating_mul(2).min(u128::from(u64::MAX)) as u64;
        if max_nanos == 0 {
            return Duration::from_millis(50);
        }
        Duration::from_nanos(self.next_u64() % max_nanos).max(Duration::from_millis(50))
    }
}

#[derive(Debug)]
struct TargetOrder {
    targets: Vec<IpAddr>,
    cursor: usize,
}

impl TargetOrder {
    fn new(targets: Vec<IpAddr>, rng: &mut ProbeRng) -> Self {
        let mut order = Self { targets, cursor: 0 };
        order.shuffle(rng);
        order
    }

    fn next(&mut self, rng: &mut ProbeRng) -> IpAddr {
        if self.cursor == self.targets.len() {
            self.cursor = 0;
            self.shuffle(rng);
        }
        let target = self.targets[self.cursor];
        self.cursor += 1;
        target
    }

    fn shuffle(&mut self, rng: &mut ProbeRng) {
        for index in (1..self.targets.len()).rev() {
            let other = rng.index(index + 1);
            self.targets.swap(index, other);
        }
    }
}

fn network_properties_rule() -> Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(NETWORK_MANAGER)
        .context("NetworkManager properties rule: set sender")?
        .interface("org.freedesktop.DBus.Properties")
        .context("NetworkManager properties rule: set interface")?
        .member("PropertiesChanged")
        .context("NetworkManager properties rule: set member")?
        .path_namespace(NETWORK_MANAGER_PATH)
        .context("NetworkManager properties rule: set path namespace")?
        .build())
}

fn network_owner_rule() -> Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.DBus")
        .context("NetworkManager owner rule: set sender")?
        .interface("org.freedesktop.DBus")
        .context("NetworkManager owner rule: set interface")?
        .member("NameOwnerChanged")
        .context("NetworkManager owner rule: set member")?
        .arg(0, NETWORK_MANAGER)
        .context("NetworkManager owner rule: set service argument")?
        .build())
}

async fn read_snapshot(connection: &Connection) -> Result<NetworkSnapshot> {
    let manager = Proxy::new(
        connection,
        NETWORK_MANAGER,
        NETWORK_MANAGER_PATH,
        NETWORK_MANAGER_IFACE,
    )
    .await
    .context("create NetworkManager root proxy")?;

    let nm_state: u32 = manager
        .get_property("State")
        .await
        .context("read NetworkManager State")?;
    let nm_connectivity: u32 = manager
        .get_property("Connectivity")
        .await
        .context("read NetworkManager Connectivity")?;
    let primary: OwnedObjectPath = manager
        .get_property("PrimaryConnection")
        .await
        .context("read NetworkManager PrimaryConnection")?;
    let connection_type: String = manager
        .get_property("PrimaryConnectionType")
        .await
        .context("read NetworkManager PrimaryConnectionType")?;

    if primary.as_str() == "/" || nm_state < 50 {
        return Ok(NetworkSnapshot {
            link: Link::None,
            primary_connection: None,
            device: None,
            access_point: None,
            nm_state,
            nm_connectivity,
        });
    }

    let mut device_path = None;
    let mut access_point_path = None;
    let link = match connection_type.as_str() {
        "802-3-ethernet" => Link::Wired,
        "802-11-wireless" => {
            let active = Proxy::new(
                connection,
                NETWORK_MANAGER,
                primary.as_str(),
                ACTIVE_CONNECTION_IFACE,
            )
            .await
            .context("create NetworkManager active connection proxy")?;
            let devices: Vec<OwnedObjectPath> = active
                .get_property("Devices")
                .await
                .context("read active Wi-Fi devices")?;
            let device = devices
                .first()
                .context("active Wi-Fi connection has no device")?;
            device_path = Some(device.to_string());
            let wireless = Proxy::new(
                connection,
                NETWORK_MANAGER,
                device.as_str(),
                WIRELESS_DEVICE_IFACE,
            )
            .await
            .context("create NetworkManager wireless device proxy")?;
            let access_point: OwnedObjectPath = wireless
                .get_property("ActiveAccessPoint")
                .await
                .context("read active Wi-Fi access point")?;
            if access_point.as_str() == "/" {
                Link::Wifi { strength: 0 }
            } else {
                access_point_path = Some(access_point.to_string());
                let access_point = Proxy::new(
                    connection,
                    NETWORK_MANAGER,
                    access_point.as_str(),
                    ACCESS_POINT_IFACE,
                )
                .await
                .context("create NetworkManager access point proxy")?;
                let strength: u8 = access_point
                    .get_property("Strength")
                    .await
                    .context("read Wi-Fi signal strength")?;
                Link::Wifi { strength }
            }
        }
        _ => Link::Other,
    };

    Ok(NetworkSnapshot {
        link,
        primary_connection: Some(primary.to_string()),
        device: device_path,
        access_point: access_point_path,
        nm_state,
        nm_connectivity,
    })
}

async fn ping(target: IpAddr, timeout: Duration) -> Result<ProbeResult> {
    let timeout_seconds = timeout.as_secs().max(1).to_string();
    let started = Instant::now();
    let mut command = Command::new("ping");
    command
        .kill_on_drop(true)
        .args(["-n", "-c", "1", "-W", &timeout_seconds])
        .arg(target.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = tokio::time::timeout(timeout + Duration::from_secs(1), command.status())
        .await
        .context("ping process exceeded its deadline")?
        .with_context(|| format!("launch ping for {target}"))?;
    Ok(ProbeResult {
        target,
        success: status.success(),
        elapsed: started.elapsed(),
    })
}

fn send_status(bus: &Bus, snapshot: &NetworkSnapshot, health: &ProbeHealth) {
    let text = display_text(&snapshot.link, health.reachability);
    if let Err(error) = bus.send_network_update(text.clone()) {
        error!(%error, "Failed to send network update");
    } else {
        debug!(label = text, "Sent network update");
    }
}

async fn monitor_network(bus: &Bus, config: &NetworkConfig) -> Result<()> {
    let connection = Connection::system()
        .await
        .context("connect network monitor to system D-Bus")?;
    let dbus = fdo::DBusProxy::new(&connection).await?;
    for rule in [network_properties_rule()?, network_owner_rule()?] {
        dbus.add_match_rule(rule)
            .await
            .context("register NetworkManager match rule")?;
    }
    let mut stream = zbus::MessageStream::from(&connection);

    let mut snapshot = match read_snapshot(&connection).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            warn!(error = %format_args!("{error:#}"), "Initial NetworkManager snapshot failed");
            NetworkSnapshot::disconnected()
        }
    };
    let mut health = ProbeHealth::new();
    if !snapshot.has_network() {
        health.reset_for_no_network();
    }
    send_status(bus, &snapshot, &health);

    let mut rng = ProbeRng::seeded();
    let mut targets = TargetOrder::new(config.ping_targets.clone(), &mut rng);
    let mut next_probe = Instant::now();

    loop {
        let sleep_until = if snapshot.has_network() {
            next_probe
        } else {
            Instant::now() + Duration::from_secs(24 * 60 * 60)
        };
        tokio::select! {
            message = stream.next() => {
                let Some(message) = message else {
                    anyhow::bail!("NetworkManager D-Bus message stream ended");
                };
                let message = message.context("receive NetworkManager D-Bus signal")?;
                let header = message.header();
                let member = header.member().map(|member| member.as_str()).unwrap_or_default();
                let interface = header.interface().map(|interface| interface.as_str()).unwrap_or_default();
                let path = header.path().map(|path| path.as_str()).unwrap_or_default();

                if interface == "org.freedesktop.DBus" && member == "NameOwnerChanged" {
                    let Ok((name, _old_owner, new_owner)) = message.body().deserialize::<(String, String, String)>() else {
                        warn!("Malformed NetworkManager NameOwnerChanged signal");
                        continue;
                    };
                    if name == NETWORK_MANAGER && new_owner.is_empty() {
                        snapshot = NetworkSnapshot::disconnected();
                        health.reset_for_no_network();
                        send_status(bus, &snapshot, &health);
                    }
                    if name != NETWORK_MANAGER || new_owner.is_empty() {
                        continue;
                    }
                } else if !snapshot.event_path_is_relevant(path) {
                    continue;
                }

                let previous = snapshot.clone();
                match read_snapshot(&connection).await {
                    Ok(updated) => snapshot = updated,
                    Err(error) => {
                        warn!(error = %format_args!("{error:#}"), "NetworkManager event resnapshot failed");
                        continue;
                    }
                }
                let now = Instant::now();
                if !snapshot.has_network() {
                    health.reset_for_no_network();
                } else if !previous.same_connection(&snapshot) {
                    health.connection_changed(now, config);
                    next_probe = now;
                } else if snapshot.nm_state < 70 || snapshot.nm_connectivity == 2 || snapshot.nm_connectivity == 3 {
                    health.unstable_until = Some(now + config.recent_instability);
                    next_probe = now;
                }
                send_status(bus, &snapshot, &health);
            }
            _ = tokio::time::sleep_until(sleep_until.into()) => {
                if !snapshot.has_network() {
                    continue;
                }
                let target = targets.next(&mut rng);
                match ping(target, config.ping_timeout).await {
                    Ok(result) => {
                        let now = Instant::now();
                        info!(
                            target = %result.target,
                            success = result.success,
                            elapsed_ms = result.elapsed.as_millis(),
                            "Network reachability probe completed"
                        );
                        health.record(&result, now, config);
                        send_status(bus, &snapshot, &health);
                        let expected = health.expected_interval(&snapshot.link, now, config);
                        next_probe = now + rng.jittered(expected);
                    }
                    Err(error) => {
                        warn!(target = %target, error = %format_args!("{error:#}"), "Network probe could not run");
                        next_probe = Instant::now() + rng.jittered(config.unstable_mean);
                    }
                }
            }
        }
    }
}

pub async fn run_network_monitor_supervised(bus: Bus, config: NetworkConfig) {
    let mut delay = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        info!("Starting network monitor");
        if let Err(error) = monitor_network(&bus, &config).await {
            error!(error = %format_args!("{error:#}"), "Network monitor stopped");
        }
        if started.elapsed() >= Duration::from_secs(30) {
            delay = Duration::from_secs(1);
        }
        warn!(restart_delay = ?delay, "Restarting network monitor");
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> NetworkConfig {
        NetworkConfig {
            ping_targets: ["1.1.1.1", "1.0.0.1"]
                .into_iter()
                .map(|target| target.parse().expect("test IP"))
                .collect(),
            ..NetworkConfig::default()
        }
    }

    #[test]
    fn labels_cover_link_and_reachability_states() {
        assert_eq!(
            display_text(&Link::None, Reachability::Offline),
            format!("{ICON_NETWORK_OFF} ×")
        );
        assert_eq!(
            display_text(&Link::Wired, Reachability::Online),
            format!("{ICON_ETHERNET} {ICON_GLOBE}")
        );
        assert_eq!(
            display_text(&Link::Wifi { strength: 73 }, Reachability::Unknown),
            format!("{ICON_WIFI_3} 73% ?")
        );
        assert_eq!(
            display_text(&Link::Wifi { strength: 28 }, Reachability::Online),
            format!("{ICON_WIFI_1} 28% {ICON_GLOBE}")
        );
        assert_eq!(
            display_text(&Link::Other, Reachability::Offline),
            format!("{ICON_NETWORK} ×")
        );
    }

    #[test]
    fn all_targets_and_confirmation_window_are_required_for_offline() {
        let config = test_config();
        let started = Instant::now();
        let mut health = ProbeHealth::new();
        let failed = |target: &str, elapsed| ProbeResult {
            target: target.parse().expect("test IP"),
            success: false,
            elapsed,
        };

        health.record(&failed("1.1.1.1", Duration::from_secs(2)), started, &config);
        health.record(
            &failed("1.0.0.1", Duration::from_secs(2)),
            started + Duration::from_secs(14),
            &config,
        );
        assert_eq!(health.reachability, Reachability::Unknown);
        health.record(
            &failed("1.0.0.1", Duration::from_secs(2)),
            started + Duration::from_secs(15),
            &config,
        );
        assert_eq!(health.reachability, Reachability::Offline);
    }

    #[test]
    fn success_immediately_restores_online_and_tracks_latency() {
        let config = test_config();
        let now = Instant::now();
        let mut health = ProbeHealth::new();
        health.first_failure = Some(now - Duration::from_secs(20));
        health
            .failed_targets
            .extend(config.ping_targets.iter().copied());
        health.reachability = Reachability::Offline;
        health.record(
            &ProbeResult {
                target: config.ping_targets[0],
                success: true,
                elapsed: Duration::from_millis(25),
            },
            now,
            &config,
        );
        assert_eq!(health.reachability, Reachability::Online);
        assert_eq!(health.latency_ewma, Some(Duration::from_millis(25)));
        assert!(health.failed_targets.is_empty());
    }

    #[test]
    fn weak_wifi_and_high_latency_increase_probe_rate() {
        let config = test_config();
        let now = Instant::now();
        let mut health = ProbeHealth::new();
        health.reachability = Reachability::Online;
        assert_eq!(
            health.expected_interval(&Link::Wired, now, &config),
            Duration::from_secs(60)
        );
        assert_eq!(
            health.expected_interval(&Link::Wifi { strength: 50 }, now, &config),
            Duration::from_secs(20)
        );
        assert_eq!(
            health.expected_interval(&Link::Wifi { strength: 20 }, now, &config),
            Duration::from_secs(6)
        );
        health.latency_ewma = Some(Duration::from_millis(600));
        assert_eq!(
            health.expected_interval(&Link::Wired, now, &config),
            Duration::from_secs(6)
        );
    }

    #[test]
    fn randomized_delay_is_bounded_and_not_constant() {
        let mut rng = ProbeRng::from_seed(42);
        let samples: Vec<_> = (0..20)
            .map(|_| rng.jittered(Duration::from_secs(1)))
            .collect();
        assert!(
            samples
                .iter()
                .all(|sample| *sample >= Duration::from_millis(50))
        );
        assert!(
            samples
                .iter()
                .all(|sample| *sample < Duration::from_secs(2))
        );
        assert!(samples.windows(2).any(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn shuffled_target_order_covers_every_target_per_cycle() {
        let config = test_config();
        let mut rng = ProbeRng::from_seed(7);
        let mut order = TargetOrder::new(config.ping_targets.clone(), &mut rng);
        let first_cycle: HashSet<_> = (0..config.ping_targets.len())
            .map(|_| order.next(&mut rng))
            .collect();
        assert_eq!(first_cycle.len(), config.ping_targets.len());
    }
}
