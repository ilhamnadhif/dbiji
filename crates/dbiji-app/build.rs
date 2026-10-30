//! On Windows: dbiji.exe's icon and version information (Explorer, the
//! taskbar, Start menu shortcuts, the installer).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
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
