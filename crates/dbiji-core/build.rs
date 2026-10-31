//! The Indonesian translations, zlib-compressed into OUT_DIR (a third of the
//! size in the binary; inflated once, when Indonesian is chosen).

use std::io::Write;

fn main() {
    println!("cargo:rerun-if-changed=locales/id.tsv");
    let tsv = std::fs::read("locales/id.tsv").expect("locales/id.tsv");
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&tsv).expect("compress");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("id.tsv.zz");
    std::fs::write(out, z.finish().expect("compress")).expect("write id.tsv.zz");
}
