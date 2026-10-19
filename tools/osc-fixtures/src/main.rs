//! Writes binary OSC fixture files for the C# codec's tests.
//!
//! Every well-formed fixture is produced by `simply-osc` — the crate the
//! hub uses on the wire — so the C# decoder is tested against exactly the
//! bytes tussen's Rust side emits. Malformed fixtures are raw bytes,
//! hand-built, each wrong in one specific way.
//!
//! Run with `cargo run` from tools/osc-fixtures/; output goes to
//! tools/osc-fixtures/fixtures/ plus MANIFEST.md describing each file.
//! Deterministic: same code, same bytes, so the fixtures are committed.

use simply_osc::{Bundle, Message, Packet, TimeTag, Value};
use std::fs;
use std::path::Path;

struct Entry {
    name: String,
    bytes: Vec<u8>,
    desc: String,
}

fn msg(address: &str, args: Vec<Value>) -> Packet {
    Packet::Message(Message { address: address.into(), args })
}

fn main() {
    let out_dir = Path::new("fixtures");
    fs::create_dir_all(out_dir).expect("create fixtures dir");

    let mut entries: Vec<Entry> = Vec::new();
    let mut add = |name: &str, packet: &Packet, desc: &str| {
        let bytes = packet.encode().expect("encode");
        entries.push(Entry { name: name.into(), bytes, desc: desc.into() });
    };

    // --- single messages, one per type the codec speaks ---
    add("msg_int.osc", &msg("/test/int", vec![Value::Int32(42)]), "message, one int32: /test/int 42");
    add(
        "msg_int_negative.osc",
        &msg("/test/int", vec![Value::Int32(-1234567)]),
        "message, one negative int32: /test/int -1234567",
    );
    add(
        "msg_float.osc",
        &msg("/test/float", vec![Value::Float32(3.14159274)]),
        "message, one float32: /test/float 3.14159274 (bits 0x40490FDB)",
    );
    add(
        "msg_string.osc",
        &msg("/test/string", vec![Value::Str("hello tussen".into())]),
        "message, one string: /test/string \"hello tussen\" (12 chars, pads to 16)",
    );
    add(
        "msg_blob.osc",
        &msg("/test/blob", vec![Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x05])]),
        "message, one blob of 5 bytes DE AD BE EF 05 (pads to 8)",
    );
    add("msg_noargs.osc", &msg("/test/ping", vec![]), "message, no arguments: /test/ping");
    add(
        "msg_multi.osc",
        &msg(
            "/test/multi",
            vec![
                Value::Int32(7),
                Value::Float32(0.25),
                Value::Str("x".into()),
                Value::Blob(vec![1, 2, 3]),
            ],
        ),
        "message, four args: int32 7, float32 0.25, string \"x\", blob [1,2,3]",
    );
    add(
        "msg_signal.osc",
        &msg("/tussen/quest/controller/trigger", vec![Value::Float32(0.82)]),
        "a realistic tussen signal: /tussen/quest/controller/trigger 0.82",
    );
    // Extended tags the decoder must tolerate and skip, with a float after
    // them so the test can prove the skip kept arguments aligned.
    add(
        "msg_extended.osc",
        &msg(
            "/test/extended",
            vec![
                Value::Bool(true),
                Value::Bool(false),
                Value::Nil,
                Value::Int64(123_456_789_012),
                Value::Double(2.5),
                Value::Time(TimeTag::from_parts(0x83AA_7E80, 0)),
                Value::Float32(0.75),
            ],
        ),
        "message with T,F,N,h,d,t args then float32 0.75 — decoder skips to the float",
    );

    // --- bundles ---
    add(
        "bundle_simple.osc",
        &Packet::Bundle(Bundle {
            time: TimeTag::IMMEDIATE,
            elements: vec![
                msg("/a", vec![Value::Int32(1)]),
                msg("/b", vec![Value::Float32(2.0)]),
            ],
        }),
        "bundle (immediate) of two messages: /a 1, /b 2.0",
    );
    add(
        "bundle_nested.osc",
        &Packet::Bundle(Bundle {
            time: TimeTag::from_parts(0x83AA_7E80, 0),
            elements: vec![
                msg("/outer", vec![Value::Int32(1)]),
                Packet::Bundle(Bundle {
                    time: TimeTag::from_parts(0x83AA_7E81, 0x8000_0000),
                    elements: vec![
                        msg("/inner/a", vec![Value::Float32(0.5)]),
                        msg("/inner/b", vec![Value::Str("deep".into())]),
                    ],
                }),
                msg("/outer/tail", vec![Value::Int32(2)]),
            ],
        }),
        "nested bundle: outer tag (0x83AA7E80,0) holds /outer, an inner bundle tag (0x83AA7E81,0x80000000) holding /inner/a and /inner/b, then /outer/tail — decode flattens to 4 messages with innermost tags",
    );

    // 60 short float signals in one bundle, near the 1200-byte send limit.
    let sixty: Vec<Packet> = (0..60)
        .map(|i| msg(&format!("/b/{i:02}"), vec![Value::Float32(i as f32 / 60.0)]))
        .collect();
    add(
        "bundle_60.osc",
        &Packet::Bundle(Bundle { time: TimeTag::IMMEDIATE, elements: sixty }),
        "bundle of 60 float messages /b/00../b/59 with value i/60 — near the 1200 B split limit",
    );

    // --- malformed packets: each wrong in exactly one way ---
    let mut add_bad = |name: &str, bytes: Vec<u8>, desc: &str| {
        entries.push(Entry { name: name.into(), bytes, desc: desc.into() });
    };
    add_bad("bad_empty.osc", vec![], "malformed: zero bytes");
    add_bad(
        "bad_notosc.osc",
        b"hello world!".to_vec(),
        "malformed: first byte is neither '/' nor '#'",
    );
    add_bad(
        "bad_unaligned.osc",
        b"/a\0\0,i\0\0\x00\x00\x2A".to_vec(),
        "malformed: /a ,i but only 3 of the int's 4 bytes — length 11, not a multiple of 4",
    );
    add_bad(
        "bad_truncated_args.osc",
        b"/a\0\0,if\0\x00\x00\x00\x01".to_vec(),
        "malformed: type tags promise int+float, bytes hold only the int",
    );
    add_bad(
        "bad_blob_len.osc",
        b"/a\0\0,b\0\0\x00\x00\x10\x00".to_vec(),
        "malformed: blob claims 4096 bytes, none follow",
    );
    add_bad(
        "bad_blob_negative.osc",
        b"/a\0\0,b\0\0\xFF\xFF\xFF\xFF".to_vec(),
        "malformed: blob length is negative (-1)",
    );
    add_bad(
        "bad_tags_nocomma.osc",
        b"/a\0\0if\0\0\x00\x00\x00\x01\x3F\x00\x00\x00".to_vec(),
        "malformed: type tag string does not start with ','",
    );
    {
        // bundle element size runs past the end of the packet
        let mut b = b"#bundle\0".to_vec();
        b.extend_from_slice(&1u64.to_be_bytes()); // immediate
        b.extend_from_slice(&64i32.to_be_bytes()); // element claims 64 bytes
        b.extend_from_slice(b"/a\0\0,\0\0\0"); // only 8 follow
        add_bad(
            "bad_bundle_elemsize.osc",
            b,
            "malformed: bundle element size (64) runs past the packet end",
        );
    }
    add_bad(
        "bad_bundle_header.osc",
        b"#bundel\0\x00\x00\x00\x00\x00\x00\x00\x01".to_vec(),
        "malformed: '#' not followed by 'bundle\\0'",
    );
    add_bad(
        "bad_bundle_truncated.osc",
        b"#bundle\0\x00\x00\x00\x00".to_vec(),
        "malformed: bundle header cut off inside the timetag",
    );
    add_bad(
        "bad_address_unterminated.osc",
        b"/abc".to_vec(),
        "malformed: address string never null-terminated",
    );
    add_bad(
        "bad_unknown_tag.osc",
        b"/a\0\0,q\0\0\x00\x00\x00\x00".to_vec(),
        "malformed: unknown type tag 'q' — size unknowable, decoder must reject",
    );

    // --- write files + manifest ---
    let mut manifest = String::from(
        "# OSC fixtures\n\nGenerated by `cargo run` in tools/osc-fixtures/ — \
         well-formed files are the bytes `simply-osc` encodes; `bad_*` files are \
         hand-built malformed packets the decoder must reject without throwing. \
         Regenerate rather than edit.\n\n| file | bytes | contents |\n|---|---|---|\n",
    );
    for e in &entries {
        fs::write(out_dir.join(&e.name), &e.bytes).expect("write fixture");
        manifest.push_str(&format!("| `{}` | {} | {} |\n", e.name, e.bytes.len(), e.desc));
    }
    fs::write(out_dir.join("MANIFEST.md"), manifest).expect("write manifest");

    // sanity: every well-formed fixture re-parses to itself
    for e in &entries {
        let parsed = simply_osc::wire::parse(&e.bytes);
        if e.name.starts_with("bad_") {
            assert!(parsed.is_err(), "{} unexpectedly parsed", e.name);
        } else {
            let p = parsed.unwrap_or_else(|err| panic!("{} failed to parse: {err}", e.name));
            assert_eq!(p.encode().unwrap(), e.bytes, "{} round-trip", e.name);
        }
    }
    println!("wrote {} fixtures into {}", entries.len(), out_dir.display());
}
