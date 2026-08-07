//! atrium-server-text-demo — proves the M6.3 server-side text path
//! end-to-end. No fresco-text dependency, no font file shipped, no
//! client-side atlas.

use fresco_client::Connection;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    let f = conn.font_open("system-mono")?;
    if f.font_id == 0 {
        return Err("server could not open 'system-mono'".into());
    }
    eprintln!(
        "font_id={} units_per_em={} ascent={} descent={}",
        f.font_id, f.units_per_em, f.ascent_units, f.descent_units,
    );

    let text = "Hello from frescod!";
    let m = conn.text_measure(f.font_id, 64.0, 400, text)?;
    eprintln!(
        "measured '{text}' @ 64px → width={:.1} ascent={:.1} descent={:.1}",
        m.width_px, m.ascent_px, m.descent_px,
    );

    conn.scene_frame_begin()?;
    conn.text_run_install(
        /*node_id=*/ 200,
        f.font_id, /*size_px=*/ 64.0,
        /*x=*/ 80.0, /*y=*/ 400.0,
        /*color=*/ [1.0, 1.0, 1.0, 1.0],
        text,
    )?;
    conn.scene_frame_end()?;
    eprintln!("installed text run '{text}'");

    std::thread::sleep(std::time::Duration::from_secs(3600));
    Ok(())
}
