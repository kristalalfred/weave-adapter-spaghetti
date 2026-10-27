use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use weave_core::NodeConfig;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterConfig {
    pub node: NodeConfig,
    #[serde(default)]
    pub spaghetti: SpaghettiSection,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpaghettiSection {
    pub socket: PathBuf,
    pub poll_interval_ms: u64,
}

impl Default for SpaghettiSection {
    fn default() -> Self {
        Self {
            socket: PathBuf::from(spaghetti_common::ipc_protocol::SOCKET_PATH),
            poll_interval_ms: 1000,
        }
    }
}

impl SpaghettiSection {
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms.max(100))
    }
}

impl AdapterConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Self = serde_norway::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        config
            .node
            .validate()
            .with_context(|| format!("validating node config {}", path.display()))?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weave_core::{NetworkAttachment, NetworkListeners};

    const MINIMAL: &str = r"
node:
  id: alfreds-mac
  southbound_url: http://127.0.0.1:8081
  listen: 127.0.0.1:8092
  topology:
    site:
      id: lan
      listeners:
        srt:
          port_range: { start: 23000, end: 23099 }
";

    const INSTALLED: &str = r"
id: alfreds-mac
southbound_url: http://192.168.1.10:29081
southbound_token: alfreds-mac.0.c0ffee
listen: 127.0.0.1:8092
topology:
  site:
    id: lan
    listeners:
      srt:
        port_range: { start: 23000, end: 23099 }
";

    fn installed_config(relay_network: &str) -> NodeConfig {
        installed_config_for("http://192.168.1.10:29081", relay_network)
    }

    fn run_installer(placeholders: &[(&str, &str)], call: &str, args: &[&str]) -> String {
        let mut script = include_str!("../install.sh").to_string();
        for (placeholder, value) in placeholders {
            script = script.replace(placeholder, value);
        }
        let script = script
            .strip_suffix("main \"$@\"\n")
            .expect("install.sh ends by running main");
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("{script}{call}\n"))
            .arg("install.sh")
            .args(args)
            .output()
            .expect("running bash");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("UTF-8 output")
    }

    fn installed_config_for(southbound_url: &str, relay_network: &str) -> NodeConfig {
        let text = run_installer(
            &[
                ("__SOUTHBOUND_URL__", southbound_url),
                ("__RELAY_NETWORK__", relay_network),
            ],
            "node_config \"$@\"",
            &["alfreds-mac", "alfreds-mac.0.c0ffee"],
        );
        let config: AdapterConfig =
            serde_norway::from_str(&text).unwrap_or_else(|error| panic!("{error}\n{text}"));
        assert_eq!(config.node.validate(), Ok(()), "{text}");
        config.node
    }

    #[test]
    fn without_a_relay_network_the_installer_declares_only_a_site() {
        let expected: NodeConfig = serde_norway::from_str(INSTALLED).unwrap();
        assert_eq!(installed_config(""), expected);
    }

    #[test]
    fn a_relay_network_adds_a_dialing_attachment_next_to_the_site() {
        let mut expected: NodeConfig = serde_norway::from_str(INSTALLED).unwrap();
        expected.topology.attachments.push(NetworkAttachment {
            id: "relay".to_string(),
            network: "bench-lan".to_string(),
            dial: true,
            listeners: NetworkListeners::default(),
        });
        assert_eq!(installed_config("bench-lan"), expected);
    }

    #[test]
    fn an_installer_for_a_public_southbound_points_the_node_at_it() {
        let url = "https://weave-southbound.example.com:8443";
        assert_eq!(installed_config_for(url, "").southbound_url, url);
    }

    const BENCH: &str = "http://alfreds-macbook-pro.local:29081";

    fn southbound_chosen(url: &str, address: &str, this_mac: &str, answers: bool) -> String {
        let stubs = format!("scutil() {{ echo {this_mac}; }}\ncurl() {{ {answers}; }}");
        run_installer(
            &[
                ("__SOUTHBOUND_URL__", url),
                ("__SOUTHBOUND_ADDRESS__", address),
            ],
            &format!("{stubs}\nchoose_southbound >/dev/null\nprintf %s \"$southbound_url\""),
            &[],
        )
    }

    #[test]
    fn the_bench_macs_own_node_reaches_southbound_over_loopback() {
        assert_eq!(
            southbound_chosen(BENCH, "10.4.0.86", "Alfreds-MacBook-Pro", true),
            "http://127.0.0.1:29081"
        );
    }

    #[test]
    fn other_macs_reach_the_bench_by_its_bonjour_name() {
        assert_eq!(
            southbound_chosen(BENCH, "10.4.0.86", "Ludvigs-MacBook-Pro", true),
            BENCH
        );
    }

    #[test]
    fn a_mac_that_cannot_reach_the_bench_by_name_dials_its_address() {
        assert_eq!(
            southbound_chosen(BENCH, "10.4.0.86", "Ludvigs-MacBook-Pro", false),
            "http://10.4.0.86:29081"
        );
    }

    #[test]
    fn a_southbound_without_an_address_is_kept_when_it_does_not_answer() {
        let url = "https://weave-southbound.example.com:8443";
        assert_eq!(
            southbound_chosen(url, "", "Ludvigs-MacBook-Pro", false),
            url
        );
    }

    #[test]
    fn the_example_config_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/spaghetti-node.yaml");
        let config = AdapterConfig::load(&path).unwrap_or_else(|error| panic!("{error:#}"));
        assert!(config.node.topology.site.is_some());
    }

    #[test]
    fn a_site_takes_no_network_host_or_dial() {
        for (line, refused) in [
            ("      id: lan", "      id: lan\n      network: office"),
            ("      id: lan", "      id: lan\n      dial: true"),
            (
                "          port_range: { start: 23000, end: 23099 }",
                "          host: 192.168.1.20\n          port_range: { start: 23000, end: 23099 }",
            ),
        ] {
            let text = MINIMAL.replace(line, refused);
            assert!(
                serde_norway::from_str::<AdapterConfig>(&text).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn the_spaghetti_section_defaults_to_the_daemons_socket() {
        let config: AdapterConfig = serde_norway::from_str(MINIMAL).unwrap();
        assert_eq!(config.spaghetti.socket, Path::new("/tmp/spaghetti.sock"));
        assert_eq!(config.spaghetti.poll_interval(), Duration::from_secs(1));
    }

    #[test]
    fn unknown_keys_are_refused() {
        let text = format!("{MINIMAL}spaghetti:\n  sockett: /tmp/x\n");
        assert!(serde_norway::from_str::<AdapterConfig>(&text).is_err());
    }
}
