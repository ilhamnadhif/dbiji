//! CHANGELOG.md, zlib-compressed into OUT_DIR (the release notes; a third of
//! the size in the binary). On Windows: dbiji.exe's icon and version
//! information (Explorer, the taskbar, Start menu shortcuts, the installer).

use std::io::Write;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../CHANGELOG.md");
    let notes = std::fs::read("../../CHANGELOG.md").expect("CHANGELOG.md");
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&notes).expect("compress");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("CHANGELOG.md.zz");
    std::fs::write(out, z.finish().expect("compress")).expect("write CHANGELOG.md.zz");
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../../packaging/windows/dbiji.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/windows/dbiji.ico");
        res.set("ProductName", "DBiji");
        res.set("FileDescription", "DBiji");
        res.compile().expect("Windows resources (icon, version)");
    }
}
