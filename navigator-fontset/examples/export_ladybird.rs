//! Export only address-verified canonical SFNTs for the engine's compiled-in provider.
use std::{env, fs, io, path::PathBuf};

fn main() -> io::Result<()> {
    let dir = PathBuf::from(
        env::args_os()
            .nth(1)
            .ok_or_else(|| io::Error::other("missing output directory"))?,
    );
    let set = navigator_fontset::FontSet::load().map_err(io::Error::other)?;
    fs::create_dir_all(&dir)?;
    let mut declarations = String::new();
    let mut entries = String::new();
    for (i, face) in set.faces.iter().enumerate() {
        let file = format!("{}.sfnt", face.address.trim_start_matches("blake3:"));
        fs::write(dir.join(&file), &face.bytes)?;
        declarations += &format!("static constexpr u8 face_{i}[] = {{\n#embed \"{file}\"\n}};\n");
        entries += &format!(
            "    {{ \"{}\"sv, {}, {}, face_{i} }},\n",
            face.name, face.weight, face.italic
        );
    }
    fs::write(
        dir.join("PinnedFaces.inc"),
        format!("{declarations}\nstatic const FontSource sources[] = {{\n{entries}}};\n"),
    )
}
