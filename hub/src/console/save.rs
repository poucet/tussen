//! Save: write the two sync points, the devices' follow
//! switches and the remap rules back into the config file the hub was
//! uses (never the repo's shipped example: see [`save_config`]) — `[sync]` and `[[remap]]` — through
//! `toml_edit`, so every comment and the rest of the file's layout
//! survive. Values that already exist keep their trailing comments.
//! The write goes to a sibling temp file first and is renamed over the
//! original, so a crash mid-save never leaves half a config. Keys of the
//! old per-destination model (`device_trim_ms`, `[sync.dest]`, numbers in
//! `[sync.device]`) are removed on save.

use std::path::Path;

use anyhow::Context;
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

use crate::config_file::ConfigFile;
use crate::sync::{Follow, SyncState};

/// Save into the config in use — or, if that is the repo's shipped
/// example, into the user config (a copy of it, then the edit), which the
/// hub uses from then on. Returns the file written.
pub fn save_config<'a>(config: &'a mut ConfigFile, state: &SyncState) -> anyhow::Result<&'a Path> {
    let path = config.save_target()?;
    save(path, state)?;
    Ok(path)
}

/// Read `path`, update its `[sync]` + `[[remap]]`, write it back.
pub fn save(path: &Path, state: &SyncState) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let out = write_into(&text, state)?;
    let tmp = path.with_extension("toml.saving");
    std::fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// The config text with `[sync]` and `[[remap]]` replaced by `state`.
pub fn write_into(text: &str, state: &SyncState) -> anyhow::Result<String> {
    let mut doc: DocumentMut = text.parse().context("parsing the config for save")?;

    let sync = table_at(doc.as_table_mut(), "sync", false);
    sync.remove("device_trim_ms");
    sync.remove("dest");
    set_int(sync, "signal_delay_ms", state.sync.signal_delay_ms);
    set_int(sync, "td_round_trip_ms", state.sync.td_round_trip_ms);
    // Devices on the default (td) need no table; a table that only held
    // `follow` goes, one with anything else of the user's stays.
    let device = table_at(sync, "device", true);
    device.set_implicit(true);
    let stale: Vec<String> = device
        .iter()
        .filter(|(k, v)| !v.is_table() || !state.sync.device.contains_key(*k))
        .map(|(k, _)| k.to_owned())
        .collect();
    for k in stale {
        let empty_after = device.get_mut(&k).and_then(Item::as_table_mut).is_none_or(|t| {
            t.remove("follow");
            t.is_empty()
        });
        if empty_after {
            device.remove(&k);
        }
    }
    for (name, d) in &state.sync.device {
        if d.follow != Follow::default() {
            set_str(table_at(device, name, false), "follow", d.follow.as_str());
        }
    }
    if device.is_empty() {
        sync.remove("device");
    }

    let mut remaps = ArrayOfTables::new();
    for r in &state.rules {
        let mut t = Table::new();
        t.insert("id", value(r.id as i64));
        t.insert("dest", value(r.dest.as_str()));
        t.insert("from", value(r.from.as_str()));
        t.insert("to", value(r.to.as_str()));
        if let Some([lo, hi]) = r.range {
            let mut a = Array::new();
            a.push(lo as f64);
            a.push(hi as f64);
            t.insert("range", value(a));
        }
        t.insert("keep", value(r.keep));
        remaps.push(t);
    }
    if remaps.is_empty() {
        doc.remove("remap");
    } else {
        doc.insert("remap", Item::ArrayOfTables(remaps));
    }
    Ok(doc.to_string())
}

/// `parent[key]` as a table, created if missing. `implicit` = don't emit
/// a header of its own when it only holds subtables (`[sync.dest]`).
fn table_at<'a>(parent: &'a mut Table, key: &str, implicit: bool) -> &'a mut Table {
    let item = parent.entry(key).or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(implicit);
        Item::Table(t)
    });
    if !item.is_table() {
        // An inline table or a stray value: replace it with a real table.
        let mut t = item.as_inline_table().map(|i| i.clone().into_table()).unwrap_or_default();
        t.set_implicit(implicit);
        *item = Item::Table(t);
    }
    item.as_table_mut().expect("just made it a table")
}

fn set_int(t: &mut Table, key: &str, v: u32) {
    set_value(t, key, value(v as i64));
}

fn set_str(t: &mut Table, key: &str, v: &str) {
    set_value(t, key, value(v));
}

/// Set a value, keeping the old value's decor (its trailing comment).
fn set_value(t: &mut Table, key: &str, new: Item) {
    match t.get_mut(key) {
        Some(item) if item.is_value() => {
            let decor = item.as_value().map(|old| old.decor().clone());
            *item = new;
            if let (Some(decor), Some(new)) = (decor, item.as_value_mut()) {
                *new.decor_mut() = decor;
            }
        }
        _ => {
            t.insert(key, new);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::sync::{DestKind, RemapRule, SyncConfig};

    const ORIGINAL: &str = r#"# the stage config — keep me
[osc]
listen_port = 9000   # every OSC sender sends here

[sync]
signal_delay_ms = 0          # sync point 1
td_round_trip_ms = 0         # sync point 2

[[remap]]
dest = "td"
from = "/old"
to = "/gone"

[console]
port = 9098                  # loopback only
"#;

    #[test]
    fn save_round_trips_and_keeps_comments() {
        let config: Config = toml::from_str(ORIGINAL).unwrap();
        let mut state = SyncState::new(config.sync, config.remap);
        state.set_sync(Some(70), Some(130));
        state.set_follow("phone", DestKind::Device, Follow::Music).unwrap();
        let old = state.rules[0].id;
        state.delete(old);
        state
            .upsert(RemapRule {
                id: 0,
                dest: "td".into(),
                from: "/tussen/*/controller/trigger".into(),
                to: "/td/trigger".into(),
                range: Some([0.0, 2.0]),
                keep: true,
            })
            .unwrap();

        let saved = write_into(ORIGINAL, &state).unwrap();
        // Comments, including trailing ones on rewritten values, survive.
        for comment in [
            "# the stage config — keep me",
            "# every OSC sender sends here",
            "signal_delay_ms = 70          # sync point 1",
            "td_round_trip_ms = 130         # sync point 2",
            "[sync.device.phone]\nfollow = \"music\"",
            "# loopback only",
        ] {
            assert!(saved.contains(comment), "lost {comment:?} in:\n{saved}");
        }
        assert!(!saved.contains("/gone"), "{saved}");
        assert!(!saved.contains("[sync.device]\n"), "implicit table got a header:\n{saved}");

        // Reloading gives back exactly the state that was saved.
        let reloaded: Config = toml::from_str(&saved).unwrap();
        let again = SyncState::new(reloaded.sync.clone(), reloaded.remap.clone());
        assert_eq!(again.sync, state.sync);
        assert_eq!(again.rules, state.rules);
        assert_eq!(reloaded.osc.listen_port, 9000);
        assert_eq!(reloaded.console.port, 9098);

        // Saving twice is a fixed point.
        assert_eq!(write_into(&saved, &again).unwrap(), saved);

        // Back to td: the device's table goes.
        let mut back = again.clone();
        back.set_follow("phone", DestKind::Device, Follow::Td).unwrap();
        let saved = write_into(&saved, &back).unwrap();
        assert!(!saved.contains("phone"), "{saved}");
    }

    #[test]
    fn save_replaces_the_old_per_destination_keys() {
        let old = "[sync]\ndevice_trim_ms = 30   # lag\n[sync.device]\nmidi_delay_ms = 40\nosc_delay_ms = 0\n[sync.dest.td]\nosc_delay_ms = 25\n";
        let config: Config = toml::from_str(old).unwrap();
        let mut state = SyncState::new(config.sync, config.remap);
        state.set_sync(Some(50), None);
        let saved = write_into(old, &state).unwrap();
        for gone in ["device_trim_ms", "midi_delay_ms", "osc_delay_ms", "sync.dest", "[sync.device]"] {
            assert!(!saved.contains(gone), "{gone} survived:\n{saved}");
        }
        let reloaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(reloaded.sync.signal_delay_ms, 50);
        assert_eq!(reloaded.sync.td_round_trip_ms, 0);
    }

    #[test]
    fn save_creates_the_sections_in_a_config_without_them() {
        let mut state = SyncState::new(SyncConfig::default(), vec![]);
        state.set_sync(Some(10), None);
        let saved = write_into("# hi\n[osc]\nlisten_port = 1\n", &state).unwrap();
        assert!(saved.starts_with("# hi\n[osc]\nlisten_port = 1\n"), "{saved}");
        let reloaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(reloaded.sync.signal_delay_ms, 10);
        assert!(reloaded.remap.is_empty());
    }

    #[test]
    fn saving_the_shipped_hub_toml_keeps_its_venue_lines() {
        let text = include_str!("../../../hub.toml");
        let config: Config = toml::from_str(text).unwrap();
        let mut state = SyncState::new(config.sync, config.remap);
        state.set_sync(Some(70), Some(130));
        state.set_follow("phone", DestKind::Device, Follow::Music).unwrap();
        let saved = write_into(text, &state).unwrap();
        for line in text.lines().filter(|l| l.starts_with("# ")) {
            assert!(saved.contains(line), "lost {line:?} in:\n{saved}");
        }
        assert!(saved.contains("signal_delay_ms = 70          # MIDI + OSC wait"), "{saved}");
        let reloaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(reloaded.sync, state.sync);
    }

    #[test]
    fn save_from_the_repo_fallback_goes_to_the_user_config() {
        use crate::config_file::{tests::{layout, scratch}, Source};
        let root = scratch("save-redirect");
        let env = layout(&root);
        let repo = env.repo_path().unwrap();
        std::fs::write(&repo, ORIGINAL).unwrap();
        let mut config = ConfigFile::resolve(None, &env).unwrap();
        assert_eq!(config.source, Source::Repo);
        let mut state = SyncState::new(SyncConfig::default(), vec![]);
        state.set_sync(Some(33), None);
        let written = save_config(&mut config, &state).unwrap().to_path_buf();
        assert_eq!(written, env.user_path().unwrap());
        assert_eq!(config.source, Source::User);
        assert_eq!(std::fs::read_to_string(&repo).unwrap(), ORIGINAL, "the repo file changed");
        let user = std::fs::read_to_string(&written).unwrap();
        assert!(user.contains("# the stage config — keep me"), "{user}");
        assert!(user.contains("signal_delay_ms = 33          # sync point 1"), "{user}");
        // The next save goes to the same user file.
        state.set_sync(None, Some(44));
        save_config(&mut config, &state).unwrap();
        assert_eq!(Config::load(&written).unwrap().sync.td_round_trip_ms, 44);
        assert_eq!(std::fs::read_to_string(&repo).unwrap(), ORIGINAL);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn save_from_the_built_in_example_goes_to_the_user_config() {
        use crate::config_file::{tests::scratch, Env, Source, SHIPPED};
        let root = scratch("save-builtin");
        let env = Env { xdg_config_home: Some(root.join("xdg")), home: None, cwd: root.clone(), xdg_data_home: None };
        let mut config = ConfigFile::resolve(None, &env).unwrap();
        assert_eq!(config.source, Source::Builtin);
        let mut state = SyncState::new(SyncConfig::default(), vec![]);
        state.set_sync(Some(33), None);
        let written = save_config(&mut config, &state).unwrap().to_path_buf();
        assert_eq!(written, root.join("xdg/tussen/hub.toml"));
        assert_eq!(config.source, Source::User);
        let user = std::fs::read_to_string(&written).unwrap();
        let first = SHIPPED.lines().next().unwrap();
        assert!(user.starts_with(first), "lost the shipped comments:\n{user}");
        assert_eq!(Config::load(&written).unwrap().sync.signal_delay_ms, 33);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn save_writes_the_file() {
        let dir = std::env::temp_dir().join(format!("tussen-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hub.toml");
        std::fs::write(&path, ORIGINAL).unwrap();
        let mut state = SyncState::new(SyncConfig::default(), vec![]);
        state.set_sync(None, Some(12));
        save(&path, &state).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(back.sync.td_round_trip_ms, 12);
        assert!(std::fs::read_to_string(&path).unwrap().contains("# the stage config — keep me"));
    }
}
