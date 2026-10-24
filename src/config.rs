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

    fn installed_config_for(southbound_url: &str, relay_network: &str) -> NodeConfig {
        let script = include_str!("../install.sh")
            .replace("__SOUTHBOUND_URL__", southbound_url)
            .replace("__RELAY_NETWORK__", relay_network);
        let script = script
            .strip_suffix("main \"$@\"\n")
            .expect("install.sh ends by running main");
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("{script}node_config \"$@\"\n"))
            .args(["install.sh", "alfreds-mac", "alfreds-mac.0.c0ffee"])
            .output()
            .expect("running bash");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).expect("UTF-8 config");
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
