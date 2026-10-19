//! `simply-tussen list` — what this machine offers the
//! generators: the audio devices (any name works as `[audio] device`)
//! and the MIDI input ports.

use tussen_hub::generators::audio;

pub fn run() -> anyhow::Result<()> {
    println!("Audio devices ([audio] device = \"<name>\"; \"\" = the default output):");
    let devices = audio::list_devices();
    if devices.is_empty() {
        println!("  (none)");
    }
    for d in devices {
        let kind = match (d.inputs, d.outputs) {
            (true, true) => "[in/out]",
            (true, false) => "[in]    ",
            _ => "[out]   ",
        };
        let how = if d.inputs { "captured as an input" } else { "tapped" };
        let default = if d.is_default_output { "  (default output)" } else { "" };
        println!("  {kind} {}  — {how}{default}", d.name);
    }

    println!("MIDI inputs:");
    match simply_midi::input::input_ports("simply-tussen") {
        Ok(ports) if ports.is_empty() => println!("  (none)"),
        Ok(ports) => {
            for port in ports {
                println!("  {}", port.name);
            }
        }
        Err(e) => println!("  unavailable: {e}"),
    }
    Ok(())
}
