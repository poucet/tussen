//! The console's WebSocket protocol table — field names
//! verbatim. One JSON object per frame, `t` names the message, `ts` is
//! the hub clock in microseconds.
//!
//! Details the spec pins under the table, and the hub's side of them:
//! * `range` is `[lo, hi]` (an object `{lo, hi}` is also accepted);
//! * remap ids are JSON numbers; `remap_upsert` fields are flat
//!   (`{"t":"remap_upsert","dest":…}`; nested under `rule` is tolerated);
//! * after any sync, follow or remap change the hub re-sends `hello`
//!   (full state) to every page — that is how a new rule learns its id;
//!   `dests` alone goes out when a device registers or leaves;
//! * `follow` is present on devices only; a `set_follow` naming a static
//!   destination is ignored (and the `hello` that follows says so).

use serde::Serialize;
use serde_json::{json, Value};

use super::AddrInfo;
use crate::sync::{DestKind, Follow, RemapRule};

/// Where OSC senders should aim: the hub's LAN IP(s) and listen port,
/// shown on the page with a copy button. In `hello` as `osc`.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct OscEndpoint {
    pub port: u16,
    /// LAN addresses, primary first; empty when the machine has no route.
    pub ips: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DestOut {
    pub name: String,
    pub kind: DestKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow: Option<Follow>,
}

pub fn hello(
    dests: &[DestOut],
    signal_delay_ms: u32,
    td_round_trip_ms: u32,
    remaps: &[RemapRule],
    sample_rate: u32,
    osc: &OscEndpoint,
) -> String {
    json!({
        "t": "hello",
        "sync": { "signal_delay_ms": signal_delay_ms, "td_round_trip_ms": td_round_trip_ms },
        "dests": dests,
        "remaps": remaps,
        "sample_rate": sample_rate,
        "osc": osc,
    })
    .to_string()
}

pub fn dests(dests: &[DestOut]) -> String {
    json!({ "t": "dests", "dests": dests }).to_string()
}

pub fn audio(ts: u64, blocks: &[[f32; 3]]) -> String {
    json!({ "t": "audio", "ts": ts, "blocks": blocks }).to_string()
}

pub fn midi(ts: u64, addr: &str, v: f32) -> String {
    json!({ "t": "midi", "ts": ts, "addr": addr, "v": v }).to_string()
}

pub fn osc(ts: u64, addr: &str, v: f32, src: &str) -> String {
    json!({ "t": "osc", "ts": ts, "addr": addr, "v": v, "src": src }).to_string()
}

pub fn addrs(list: &[AddrInfo]) -> String {
    json!({ "t": "addrs", "list": list }).to_string()
}

pub fn link(s: &crate::link::LinkStatus) -> String {
    json!({
        "t": "link", "ts": s.ts, "beat": s.beat, "bpm": s.bpm,
        "quantum": s.quantum, "playing": s.playing, "peers": s.peers,
    })
    .to_string()
}

pub fn saved(result: Result<&str, &str>) -> String {
    match result {
        Ok(path) => json!({ "t": "saved", "ok": true, "path": path }),
        Err(error) => json!({ "t": "saved", "ok": false, "error": error }),
    }
    .to_string()
}

/// A page → hub message.
#[derive(Debug, PartialEq)]
pub enum Request {
    Watch(Vec<String>),
    /// `set_sync`: either sync point, each optional.
    SetSync { signal_delay_ms: Option<u32>, td_round_trip_ms: Option<u32> },
    /// `set_follow`: which sync point a device is fed from.
    SetFollow { dest: String, follow: Follow },
    Upsert(RemapRule),
    Delete(u64),
    /// `set_tempo` / `set_playing`: proposed to the Link session once,
    /// when the human acts (the hub otherwise follows).
    Link(crate::link::LinkCmd),
    Save,
}

fn ms(v: &Value, key: &str) -> Result<Option<u32>, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => match x.as_f64() {
            Some(f) if f.is_finite() => Ok(Some(f.round().clamp(0.0, u32::MAX as f64) as u32)),
            _ => Err(format!("{key} must be a number")),
        },
    }
}

fn id(v: Option<&Value>) -> Result<u64, String> {
    match v {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| format!("bad id {n}")),
        Some(Value::String(s)) if s.is_empty() => Ok(0),
        Some(Value::String(s)) => s.parse().map_err(|_| format!("bad id {s:?}")),
        Some(other) => Err(format!("bad id {other}")),
    }
}

fn string(v: &Value, key: &str) -> Result<String, String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("{key} missing"))
}

fn range(v: &Value) -> Result<Option<[f32; 2]>, String> {
    let pair = match v.get("range") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(a)) if a.len() == 2 => (a[0].as_f64(), a[1].as_f64()),
        Some(Value::Object(o)) => (o.get("lo").and_then(Value::as_f64), o.get("hi").and_then(Value::as_f64)),
        Some(other) => return Err(format!("range must be [lo, hi], got {other}")),
    };
    match pair {
        (Some(lo), Some(hi)) => Ok(Some([lo as f32, hi as f32])),
        _ => Err("range must be two numbers".into()),
    }
}

/// Parse one page frame. Unknown `t` and malformed fields are errors the
/// console logs; they never reach the router.
pub fn parse_request(text: &str) -> Result<Request, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let t = v.get("t").and_then(Value::as_str).ok_or("no \"t\"")?;
    match t {
        "watch" => {
            let list = v.get("addrs").and_then(Value::as_array).ok_or("watch without addrs")?;
            Ok(Request::Watch(list.iter().filter_map(Value::as_str).map(str::to_owned).collect()))
        }
        "set_sync" => Ok(Request::SetSync {
            signal_delay_ms: ms(&v, "signal_delay_ms")?,
            td_round_trip_ms: ms(&v, "td_round_trip_ms")?,
        }),
        "set_follow" => Ok(Request::SetFollow {
            dest: string(&v, "dest")?,
            follow: match v.get("follow").and_then(Value::as_str) {
                Some("td") => Follow::Td,
                Some("music") => Follow::Music,
                _ => return Err("set_follow needs follow \"td\" or \"music\"".into()),
            },
        }),
        "remap_upsert" => {
            let r = match v.get("rule").or_else(|| v.get("remap")) {
                Some(inner) if inner.is_object() && v.get("from").is_none() => inner,
                _ => &v,
            };
            Ok(Request::Upsert(RemapRule {
                id: id(r.get("id"))?,
                dest: string(r, "dest")?,
                from: string(r, "from")?,
                to: string(r, "to")?,
                range: range(r)?,
                keep: r.get("keep").and_then(Value::as_bool).unwrap_or(false),
            }))
        }
        "remap_delete" => Ok(Request::Delete(id(v.get("id"))?)),
        "save" => Ok(Request::Save),
        "set_tempo" => match v.get("bpm").and_then(Value::as_f64) {
            Some(bpm) if bpm.is_finite() => Ok(Request::Link(crate::link::LinkCmd::Tempo(bpm))),
            _ => Err("set_tempo needs a number bpm".into()),
        },
        "set_playing" => match v.get("playing").and_then(Value::as_bool) {
            Some(p) => Ok(Request::Link(crate::link::LinkCmd::Playing(p))),
            None => Err("set_playing needs a boolean playing".into()),
        },
        other => Err(format!("unknown message {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_messages_use_the_spec_field_names() {
        let d = [
            DestOut { name: "engine".into(), kind: DestKind::Engine, follow: None },
            DestOut { name: "quest".into(), kind: DestKind::Device, follow: Some(Follow::Music) },
        ];
        let rules = [RemapRule {
            id: 3,
            dest: "td".into(),
            from: "/a/*".into(),
            to: "/b".into(),
            range: Some([0.0, 2.0]),
            keep: true,
        }];
        let ep = OscEndpoint { port: 9000, ips: vec!["192.0.2.2".into()], hostname: Some("hub.local".into()) };
        let h: Value = serde_json::from_str(&hello(&d, 25, 140, &rules, 48000, &ep)).unwrap();
        assert_eq!(
            h,
            json!({"t":"hello","sync":{"signal_delay_ms":25,"td_round_trip_ms":140},
                   "dests":[{"name":"engine","kind":"engine"},{"name":"quest","kind":"device","follow":"music"}],
                   "remaps":[{"id":3,"dest":"td","from":"/a/*","to":"/b","range":[0.0,2.0],"keep":true}],
                   "sample_rate":48000,
                   "osc":{"port":9000,"ips":["192.0.2.2"],"hostname":"hub.local"}})
        );
        // No hostname → the key is absent, not null.
        let bare = OscEndpoint { port: 9000, ips: vec![], hostname: None };
        let h: Value = serde_json::from_str(&hello(&[], 0, 0, &[], 1, &bare)).unwrap();
        assert!(h["osc"].get("hostname").is_none());
        assert_eq!(h["osc"]["ips"], json!([]));
        let a: Value = serde_json::from_str(&audio(7, &[[-0.5, 0.5, 0.25]])).unwrap();
        assert_eq!(a, json!({"t":"audio","ts":7,"blocks":[[-0.5,0.5,0.25]]}));
        let m: Value = serde_json::from_str(&midi(8, "/tussen/midi/cc/1", 0.5)).unwrap();
        assert_eq!(m, json!({"t":"midi","ts":8,"addr":"/tussen/midi/cc/1","v":0.5}));
        let o: Value = serde_json::from_str(&osc(9, "/ext/x", 1.0, "127.0.0.1:5")).unwrap();
        assert_eq!(o, json!({"t":"osc","ts":9,"addr":"/ext/x","v":1.0,"src":"127.0.0.1:5"}));
        let l: Value =
            serde_json::from_str(&addrs(&[AddrInfo { addr: "/x".into(), last_ts: 4, rate: 2.5, v: 0.5, min: -0.5, max: 1.5, count: 12, dir: "in" }]))
                .unwrap();
        assert_eq!(l, json!({"t":"addrs","list":[{"addr":"/x","last_ts":4,"rate":2.5,"v":0.5,"min":-0.5,"max":1.5,"count":12,"dir":"in"}]}));
        let k: Value = serde_json::from_str(&link(&crate::link::LinkStatus {
            ts: 5,
            beat: 2.5,
            bpm: 120.0,
            quantum: 4.0,
            playing: true,
            peers: 1,
        }))
        .unwrap();
        assert_eq!(
            k,
            json!({"t":"link","ts":5,"beat":2.5,"bpm":120.0,"quantum":4.0,"playing":true,"peers":1})
        );
        let s: Value = serde_json::from_str(&saved(Ok("/p.toml"))).unwrap();
        assert_eq!(s, json!({"t":"saved","ok":true,"path":"/p.toml"}));
        let s: Value = serde_json::from_str(&saved(Err("nope"))).unwrap();
        assert_eq!(s, json!({"t":"saved","ok":false,"error":"nope"}));
        // No range → the key is absent, not null.
        let mut r = rules[0].clone();
        r.range = None;
        let h: Value = serde_json::from_str(&hello(&[], 0, 0, &[r], 1, &bare)).unwrap();
        assert!(h["remaps"][0].get("range").is_none());
    }

    #[test]
    fn inbound_messages_parse() {
        assert_eq!(
            parse_request(r#"{"t":"watch","addrs":["/a","/b"]}"#).unwrap(),
            Request::Watch(vec!["/a".into(), "/b".into()])
        );
        assert_eq!(
            parse_request(r#"{"t":"set_sync","signal_delay_ms":40.4}"#).unwrap(),
            Request::SetSync { signal_delay_ms: Some(40), td_round_trip_ms: None }
        );
        assert_eq!(
            parse_request(r#"{"t":"set_sync","signal_delay_ms":0,"td_round_trip_ms":130}"#).unwrap(),
            Request::SetSync { signal_delay_ms: Some(0), td_round_trip_ms: Some(130) }
        );
        assert_eq!(
            parse_request(r#"{"t":"set_follow","dest":"quest","follow":"music"}"#).unwrap(),
            Request::SetFollow { dest: "quest".into(), follow: Follow::Music }
        );
        assert!(parse_request(r#"{"t":"set_follow","dest":"quest","follow":"loud"}"#).is_err());
        assert!(parse_request(r#"{"t":"set_follow","follow":"td"}"#).is_err());
        assert!(parse_request(r#"{"t":"set_delay","dest":"td","midi_delay_ms":40}"#).is_err());
        let want = RemapRule {
            id: 0,
            dest: "td".into(),
            from: "/a".into(),
            to: "/b".into(),
            range: Some([1.0, 2.0]),
            keep: false,
        };
        assert_eq!(
            parse_request(r#"{"t":"remap_upsert","dest":"td","from":"/a","to":"/b","range":[1,2],"keep":false}"#)
                .unwrap(),
            Request::Upsert(want.clone())
        );
        assert_eq!(
            parse_request(r#"{"t":"remap_upsert","rule":{"id":5,"dest":"td","from":"/a","to":"/b","range":{"lo":1,"hi":2}}}"#)
                .unwrap(),
            Request::Upsert(RemapRule { id: 5, ..want })
        );
        assert_eq!(parse_request(r#"{"t":"remap_delete","id":5}"#).unwrap(), Request::Delete(5));
        assert_eq!(parse_request(r#"{"t":"save"}"#).unwrap(), Request::Save);
        use crate::link::LinkCmd;
        assert_eq!(parse_request(r#"{"t":"set_tempo","bpm":97.5}"#).unwrap(), Request::Link(LinkCmd::Tempo(97.5)));
        assert_eq!(
            parse_request(r#"{"t":"set_playing","playing":false}"#).unwrap(),
            Request::Link(LinkCmd::Playing(false))
        );
        assert!(parse_request(r#"{"t":"set_tempo","bpm":"fast"}"#).is_err());
        assert!(parse_request(r#"{"t":"set_playing"}"#).is_err());
        assert!(parse_request(r#"{"t":"nope"}"#).is_err());
        assert!(parse_request("not json").is_err());
        assert!(parse_request(r#"{"t":"set_sync","signal_delay_ms":"x"}"#).is_err());
    }
}
