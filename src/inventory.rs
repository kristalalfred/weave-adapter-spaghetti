use std::collections::{BTreeMap, BTreeSet};

use spaghetti_common::ipc_protocol::{AppInfo, SystemDeviceInfo};
use weave_core::{NodeInput, NodeOutput, RESOURCE_ID_MAX_LEN, Track};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inventory {
    pub inputs: Vec<NodeInput>,
    pub outputs: Vec<NodeOutput>,
    pids: BTreeMap<String, Vec<u32>>,
    devices: BTreeMap<String, String>,
}

impl Inventory {
    #[must_use]
    pub fn detect(apps: &[AppInfo], devices: &[SystemDeviceInfo], home: &str) -> Self {
        let roots = [
            "/Applications/".to_string(),
            "/System/Applications/".to_string(),
            format!("{}/Applications/", home.trim_end_matches('/')),
        ];
        let apps: Vec<&AppInfo> = apps
            .iter()
            .filter(|app| !app.pids.is_empty())
            .filter(|app| {
                app.bundle_path
                    .as_deref()
                    .is_some_and(|path| roots.iter().any(|root| path.starts_with(root.as_str())))
            })
            .collect();
        let devices: Vec<&SystemDeviceInfo> = devices.iter().filter(|d| d.is_output).collect();

        let mut inventory = Self::default();
        let named = assign(apps.iter().map(|app| {
            let key = app
                .bundle_id
                .as_deref()
                .or(app.bundle_path.as_deref())
                .unwrap_or(&app.name);
            let base = slug(&app.name)
                .or_else(|| app.bundle_id.as_deref().and_then(slug))
                .unwrap_or_else(|| "app".to_string());
            (base, key, &app.name, *app)
        }));
        for (id, label, app) in named {
            inventory.inputs.push(NodeInput {
                id: id.clone(),
                label,
                tracks: vec![Track::Audio],
            });
            inventory.pids.insert(id, app.pids.clone());
        }
        let named = assign(devices.iter().map(|device| {
            let base = slug(&device.name)
                .or_else(|| slug(&device.uid))
                .unwrap_or_else(|| "output".to_string());
            (base, device.uid.as_str(), &device.name, *device)
        }));
        for (id, label, device) in named {
            inventory.outputs.push(NodeOutput {
                id: id.clone(),
                label,
            });
            inventory.devices.insert(id, device.uid.clone());
        }
        inventory
    }

    #[must_use]
    pub fn pids(&self, input: &str) -> Option<&[u32]> {
        self.pids.get(input).map(Vec::as_slice)
    }

    #[must_use]
    pub fn device(&self, output: &str) -> Option<&str> {
        self.devices.get(output).map(String::as_str)
    }
}

fn assign<'a, T>(
    entries: impl Iterator<Item = (String, &'a str, &'a String, T)>,
) -> Vec<(String, String, T)> {
    let mut entries: Vec<_> = entries.collect();
    entries.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    let mut sharing: BTreeMap<String, usize> = BTreeMap::new();
    for (base, ..) in &entries {
        *sharing.entry(base.clone()).or_default() += 1;
    }
    let mut taken = BTreeSet::new();
    entries
        .into_iter()
        .map(|(base, key, label, item)| {
            let wanted = if sharing[&base] == 1 {
                base
            } else {
                format!("{base}-{}", short_hash(key))
            };
            let id = unique(&wanted, &mut taken);
            let label = match label.trim() {
                "" => id.clone(),
                label => label.to_string(),
            };
            (id, label, item)
        })
        .collect()
}

fn short_hash(key: &str) -> String {
    let hash = key.bytes().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    format!("{:04x}", (hash >> 16) ^ (hash & 0xffff))
}

fn slug(text: &str) -> Option<String> {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(RESOURCE_ID_MAX_LEN - 8);
    let slug = slug.trim_end_matches('-').to_string();
    (!slug.is_empty()).then_some(slug)
}

fn unique(base: &str, taken: &mut BTreeSet<String>) -> String {
    let id = (1..)
        .map(|n| {
            if n == 1 {
                base.to_string()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|id| !taken.contains(id))
        .expect("an unused suffix");
    taken.insert(id.clone());
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, bundle_path: Option<&str>, pids: &[u32]) -> AppInfo {
        AppInfo {
            name: name.to_string(),
            bundle_id: bundle_path
                .map(|_| format!("com.example.{}", slug(name).unwrap_or_default())),
            bundle_path: bundle_path.map(str::to_string),
            pids: pids.to_vec(),
        }
    }

    fn device(uid: &str, name: &str, is_output: bool) -> SystemDeviceInfo {
        SystemDeviceInfo {
            uid: uid.to_string(),
            name: name.to_string(),
            channels: 2,
            is_output,
        }
    }

    #[test]
    fn user_apps_become_audio_inputs_and_output_devices_become_outputs() {
        let inventory = Inventory::detect(
            &[
                app("Music", Some("/System/Applications/Music.app"), &[731]),
                app(
                    "Microsoft Teams",
                    Some("/Applications/Microsoft Teams.app"),
                    &[27240, 27249],
                ),
                app("Rex", Some("/Users/alfred/Applications/Rex.app"), &[6828]),
                app(
                    "ControlCenter",
                    Some("/System/Library/CoreServices/ControlCenter.app"),
                    &[738],
                ),
                app("coreaudiod", None, &[400]),
                app("Quit", Some("/Applications/Quit.app"), &[]),
            ],
            &[
                device("BuiltInSpeakerDevice", "MacBook Pro Speakers", true),
                device("BuiltInMicrophoneDevice", "MacBook Pro Microphone", false),
                device("SpaghettiDevice_UID", "Spaghetti", true),
            ],
            "/Users/alfred",
        );
        let inputs: Vec<(&str, &str)> = inventory
            .inputs
            .iter()
            .map(|input| (input.id.as_str(), input.label.as_str()))
            .collect();
        assert_eq!(
            inputs,
            [
                ("microsoft-teams", "Microsoft Teams"),
                ("music", "Music"),
                ("rex", "Rex")
            ]
        );
        assert!(
            inventory
                .inputs
                .iter()
                .all(|input| input.tracks == [Track::Audio])
        );
        assert_eq!(inventory.pids("microsoft-teams"), Some(&[27240, 27249][..]));
        assert_eq!(inventory.pids("controlcenter"), None);

        let outputs: Vec<&str> = inventory.outputs.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(outputs, ["macbook-pro-speakers", "spaghetti"]);
        assert_eq!(
            inventory.device("macbook-pro-speakers"),
            Some("BuiltInSpeakerDevice")
        );
    }

    #[test]
    fn ids_follow_the_identifier_rule_and_never_move_to_another_device() {
        let long = "A".repeat(80);
        let all = [
            device("uid-b", "Studio Display", true),
            device("uid-a", "Studio Display", true),
            device("uid-c", "✦✦✦", true),
            device("uid-d", &long, true),
            device("uid-e", " ", true),
        ];
        let inventory = Inventory::detect(&[], &all, "/Users/alfred");
        for output in &inventory.outputs {
            weave_core::validate_resource_id(&output.id).unwrap();
            assert!(!output.label.trim().is_empty(), "{output:?}");
        }
        let ids: BTreeSet<&str> = inventory.outputs.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids.len(), all.len());
        assert_eq!(inventory.device("uid-c"), Some("uid-c"));

        let id_of = |inventory: &Inventory, uid: &str| {
            inventory
                .outputs
                .iter()
                .map(|o| o.id.clone())
                .find(|id| inventory.device(id) == Some(uid))
                .unwrap()
        };
        let a = id_of(&inventory, "uid-a");
        let b = id_of(&inventory, "uid-b");
        assert!(a.starts_with("studio-display-") && b.starts_with("studio-display-"));
        assert_ne!(a, b);

        let without_b: Vec<SystemDeviceInfo> =
            all.iter().filter(|d| d.uid != "uid-b").cloned().collect();
        let after = Inventory::detect(&[], &without_b, "/Users/alfred");
        assert_eq!(
            after.device(&b),
            None,
            "the unplugged display's id names nothing"
        );
        assert_eq!(
            after.device(&a),
            None,
            "and the other display does not take it"
        );
        assert_eq!(id_of(&after, "uid-a"), "studio-display");
    }
}
