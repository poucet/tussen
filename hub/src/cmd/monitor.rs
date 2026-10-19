//! `simply-tussen monitor` — a live table of what the hub routes here:
//! address, value, msgs/s, source, and a label when `signals.yaml` knows
//! the address. Point the config's `monitor` destination at this port.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use simply_osc::{Listener, Value};
use tussen_hub::catalog::Catalog;

struct Row {
    value: String,
    count: u64,
    rate: f64,
    source: SocketAddr,
    last_seen: Instant,
}

/// Rows idle this long fall off the table.
const EXPIRE: Duration = Duration::from_secs(10);

pub fn run(port: u16, filter: Option<&str>) -> anyhow::Result<()> {
    let catalog = Catalog::find();
    let listener = Listener::bind(("0.0.0.0", port))?;
    eprintln!(
        "monitoring :{port}{}{}",
        filter.map(|f| format!(" filter {f}")).unwrap_or_default(),
        if catalog.is_empty() { "" } else { ", labels from signals.yaml" },
    );
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    let mut last_draw = Instant::now();
    let mut total = 0u64;
    loop {
        if let Some(inbound) = listener.recv_timeout(Duration::from_millis(100)) {
            let now = Instant::now();
            for (_, m) in inbound.packet.messages() {
                total += 1;
                if let Some(f) = filter {
                    if !m.address.starts_with(f) {
                        continue;
                    }
                }
                let row = rows.entry(m.address.clone()).or_insert(Row {
                    value: String::new(),
                    count: 0,
                    rate: 0.0,
                    source: inbound.from,
                    last_seen: now,
                });
                row.value = display(m.args.first());
                row.count += 1;
                row.source = inbound.from;
                row.last_seen = now;
            }
        }
        let now = Instant::now();
        let elapsed = now.duration_since(last_draw);
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            rows.retain(|_, r| now.duration_since(r.last_seen) < EXPIRE);
            for r in rows.values_mut() {
                r.rate = r.count as f64 / secs;
                r.count = 0;
            }
            draw(&rows, &catalog, total, secs, listener.dropped());
            total = 0;
            last_draw = now;
        }
    }
}

fn display(v: Option<&Value>) -> String {
    match v {
        Some(Value::Float32(f)) => format!("{f:.3}"),
        Some(Value::Int32(i)) => format!("{i}"),
        Some(Value::Str(s)) => s.clone(),
        Some(other) => match other.as_float() {
            Some(f) => format!("{f:.3}"),
            None => format!("{other:?}"),
        },
        None => String::new(),
    }
}

fn draw(rows: &BTreeMap<String, Row>, catalog: &Catalog, total: u64, secs: f64, dropped: u64) {
    // Clear screen, home cursor.
    print!("\x1b[2J\x1b[H");
    println!(
        "{:<44} {:>10} {:>8}  {:<21} {}",
        "address", "value", "msgs/s", "source", "label"
    );
    for (address, row) in rows {
        println!(
            "{:<44} {:>10} {:>8.1}  {:<21} {}",
            address,
            row.value,
            row.rate,
            row.source,
            catalog.label(address).unwrap_or(""),
        );
    }
    println!(
        "\n{} address(es) · {:.0} msgs/s in · {} malformed/overflow dropped",
        rows.len(),
        total as f64 / secs,
        dropped,
    );
}
