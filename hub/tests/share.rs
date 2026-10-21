//! Sharing a take: `export` writes one zip, `import` (and `replay
//! <file.zip>`) brings it back as an ordinary take directory, every file
//! byte for byte. A zip that isn't one of ours is refused, and a path that would
//! escape the takes directory never reaches the disk.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use simply_osc::{Message, Value};
use tussen_hub::share;
use tussen_hub::take::{self, Meta, Recorder, Take};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tussen-share-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Something to listen to: two detuned partials, a decaying burst of
/// noise every beat, a noise floor.
fn music(frame: u64, ch: usize, rng: &mut u64) -> f32 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    let noise = (*rng >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0;
    let t = frame as f32 / 48_000.0;
    let f = if ch == 0 { 220.0 } else { 220.7 };
    let tone = 0.25 * (t * f * std::f32::consts::TAU).sin() + 0.1 * (t * f * 3.01 * std::f32::consts::TAU).sin();
    let beat = (t * 2.0).fract();
    tone + 0.3 * noise * (-beat * 20.0).exp() + 0.01 * noise
}

/// A take of `secs` seconds: OSC every 10 ms, a Link point, a sync, and
/// stereo audio at 48 kHz.
fn record(takes: &Path, name: &str, secs: u64) -> PathBuf {
    let dir = takes.join(name);
    let meta = Meta { name: name.into(), sample_rate: 48000, channels: 2, packet_frames: 256, quantum: 4.0, ..Meta::default() };
    let rec = Recorder::start(&dir, meta, Some("[osc]\nlisten_port = 9000\n")).unwrap();
    let mut audio = rec.audio_tap(512);
    let t0 = rec.start_instant();
    rec.link(t0, 120.0, 0.0, true, 0, 4.0);
    rec.sync(t0, 40, 0);
    let (mut rng, mut frame) = (0x2545_f491_4f6c_dd1du64, 0u64);
    let packets = secs * 48_000 / 256;
    for p in 0..packets {
        let at = t0 + Duration::from_micros(p * 256 * 1_000_000 / 48_000);
        if p % 2 == 0 {
            rec.osc(at, None, &Message::new("/tussen/midi/cc/1").arg(Value::Float32((p % 128) as f32)));
        }
        let block: Vec<f32> = (0..512).map(|i| music(frame + i as u64 / 2, i % 2, &mut rng)).collect();
        frame += 256;
        // The ring holds ~2 s: let the writer keep up.
        while audio.would_drop(&block) {
            std::thread::sleep(Duration::from_millis(1));
        }
        audio.push(&block, at);
    }
    rec.finish();
    assert_eq!(audio.dropped, 0);
    dir
}

fn same_file(a: &Path, b: &Path, name: &str) {
    assert_eq!(fs::read(a.join(name)).unwrap(), fs::read(b.join(name)).unwrap(), "{name} differs");
}

fn samples(dir: &Path) -> Vec<f32> {
    let t = Take::open(dir).unwrap();
    let mut out = vec![0f32; (t.frames * t.meta.channels as u64) as usize];
    t.read_audio(0, &mut out);
    out
}

#[test]
fn export_then_import_gives_the_same_take() {
    let root = tmp("roundtrip");
    let orig = record(&root.join("a"), "2026-10-20T21-14-03-rehearsal", 2);
    let zip = root.join("rehearsal.zip");
    let e = share::export(&orig, &zip, true).unwrap();
    assert_eq!(e.frames, Take::open(&orig).unwrap().frames);

    let takes = root.join("b");
    let i = share::import(&zip, &takes).unwrap();
    assert!(!i.reused);
    assert_eq!(i.dir, takes.join("2026-10-20T21-14-03-rehearsal"));
    for f in ["take.toml", "events.bin", "hub.toml", "audio.wav"] {
        same_file(&orig, &i.dir, f);
    }
    assert_eq!(samples(&orig), samples(&i.dir));
    let (a, b) = (Take::open(&orig).unwrap(), Take::open(&i.dir).unwrap());
    assert_eq!((a.events.len(), a.anchors.clone(), a.frames), (b.events.len(), b.anchors.clone(), b.frames));

    // The same zip again is the same take: reused, not duplicated.
    let again = share::import(&zip, &takes).unwrap();
    assert!(again.reused);
    assert_eq!(again.dir, i.dir);
    assert_eq!(take::list(&takes).len(), 1);

    // A different take under the same name is suffixed, never overwritten.
    let other = record(&root.join("c"), "2026-10-20T21-14-03-rehearsal", 1);
    let zip2 = root.join("other.zip");
    share::export(&other, &zip2, true).unwrap();
    let j = share::import(&zip2, &takes).unwrap();
    assert_eq!(j.dir, takes.join("2026-10-20T21-14-03-rehearsal-2"));
    same_file(&other, &j.dir, "audio.wav");
    same_file(&orig, &i.dir, "audio.wav");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn no_audio_round_trips_the_events_alone() {
    let root = tmp("noaudio");
    let orig = record(&root.join("a"), "quiet", 1);
    let zip = root.join("quiet.zip");
    let e = share::export(&orig, &zip, false).unwrap();
    assert_eq!(e.frames, 0);
    let full = share::export(&orig, &root.join("full.zip"), true).unwrap();
    assert!(e.bytes * 10 < full.bytes, "{} vs {}", e.bytes, full.bytes);

    let takes = root.join("b");
    let i = share::import(&zip, &takes).unwrap();
    for f in ["take.toml", "events.bin", "hub.toml"] {
        same_file(&orig, &i.dir, f);
    }
    assert!(!i.dir.join("audio.wav").exists());
    let t = Take::open(&i.dir).unwrap();
    assert_eq!(t.frames, 0);
    assert_eq!(t.frame_at(500_000), None, "no audio to replay");
    assert_eq!(t.events.len(), Take::open(&orig).unwrap().events.len());
    // Silent and audible are not the same take: the full zip imports beside it.
    let j = share::import(&root.join("full.zip"), &takes).unwrap();
    assert!(!j.reused);
    assert_eq!(j.dir, takes.join("quiet-2"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_crash_truncated_take_exports_what_opens() {
    let root = tmp("torn");
    let orig = record(&root.join("a"), "torn", 1);
    // A crash mid-write: half a frame of audio, half a record of events.
    let wav = fs::metadata(orig.join("audio.wav")).unwrap().len();
    take::truncate(&orig.join("audio.wav"), wav - 4 * 100 - 3).unwrap();
    let log = fs::metadata(orig.join("events.bin")).unwrap().len();
    take::truncate(&orig.join("events.bin"), log - 5).unwrap();
    let opened = Take::open(&orig).unwrap();

    let zip = root.join("torn.zip");
    share::export(&orig, &zip, true).unwrap();
    let i = share::import(&zip, &root.join("b")).unwrap();
    let t = Take::open(&i.dir).unwrap();
    assert_eq!(t.frames, opened.frames);
    assert_eq!(t.events.len(), opened.events.len());
    assert_eq!(samples(&i.dir), samples(&orig));
    let data = opened.frames * 4;
    assert_eq!(fs::metadata(i.dir.join("audio.wav")).unwrap().len(), 44 + data, "whole frames, a fresh header");
    assert_eq!(&fs::read(i.dir.join("audio.wav")).unwrap()[40..44], &(data as u32).to_le_bytes());
    assert!(fs::metadata(i.dir.join("events.bin")).unwrap().len() < log - 5, "the torn record is left out");
    let _ = fs::remove_dir_all(&root);
}

fn zip_of(path: &Path, entries: &[(&str, &[u8])]) {
    let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, bytes) in entries {
        z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(bytes).unwrap();
    }
    z.finish().unwrap();
}

#[test]
fn a_zip_that_is_not_a_take_is_refused() {
    let root = tmp("refuse");
    let orig = record(&root.join("a"), "evil", 1);
    let take_toml = fs::read(orig.join("take.toml")).unwrap();
    let events = fs::read(orig.join("events.bin")).unwrap();
    let manifest = b"format = 1\ntake = \"evil\"\n";
    let takes = root.join("takes");
    let err = |entries: &[(&str, &[u8])]| {
        let z = root.join("x.zip");
        zip_of(&z, entries);
        format!("{:#}", share::import(&z, &takes).unwrap_err())
    };

    // Zip-slip: a path out of the take, or anything not in the set.
    for bad in ["../evil.toml", "/etc/evil", "sub/take.toml", "audio.flac", "../audio.wav"] {
        let e = err(&[("tussen-take.toml", manifest), ("take.toml", &take_toml), ("events.bin", &events), (bad, b"x")]);
        assert!(e.contains("unexpected entry"), "{bad}: {e}");
    }
    assert!(!root.join("evil.toml").exists());
    assert!(!takes.exists(), "nothing was written");

    let e = err(&[("take.toml", &take_toml), ("events.bin", &events)]);
    assert!(e.contains("not a tussen take zip"), "{e}");
    let e = err(&[("tussen-take.toml", b"format = 2\ntake = \"x\"\n"), ("take.toml", &take_toml)]);
    assert!(e.contains("format 2"), "{e}");
    let e = err(&[("tussen-take.toml", manifest), ("take.toml", &take_toml)]);
    assert!(e.contains("no events.bin"), "{e}");
    fs::write(root.join("plain.zip"), b"not a zip").unwrap();
    assert!(format!("{:#}", share::import(&root.join("plain.zip"), &takes).unwrap_err()).contains("not a tussen take zip"));
    // A hostile take name becomes one safe directory name.
    let z = root.join("y.zip");
    zip_of(&z, &[("tussen-take.toml", b"format = 1\ntake = \"../../up\"\n"), ("take.toml", &take_toml), ("events.bin", &events)]);
    let i = share::import(&z, &takes).unwrap();
    assert_eq!(i.dir, takes.join("------up"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn replay_takes_a_zip_from_any_path() {
    let root = tmp("resolve");
    let orig = record(&root.join("a"), "2026-10-20T21-14-03-gig", 1);
    let elsewhere = root.join("Downloads");
    fs::create_dir_all(&elsewhere).unwrap();
    let zip = elsewhere.join("gig.zip");
    share::export(&orig, &zip, true).unwrap();
    let takes = root.join("takes");
    let first = share::resolve(&takes, zip.to_str().unwrap()).unwrap();
    assert_eq!(first, takes.join("2026-10-20T21-14-03-gig"));
    assert_eq!(share::resolve(&takes, zip.to_str().unwrap()).unwrap(), first, "imported once");
    assert_eq!(take::list(&takes).len(), 1);
    // Everything else resolves as before.
    assert_eq!(share::resolve(&takes, "gig").unwrap(), first);
    assert_eq!(share::resolve(&takes, "latest").unwrap(), first);
    assert_eq!(share::resolve(&takes, orig.to_str().unwrap()).unwrap(), orig);
    assert!(share::resolve(&takes, "nope.zip").is_err());
    let _ = fs::remove_dir_all(&root);
}
