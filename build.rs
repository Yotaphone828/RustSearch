fn main() {
    println!("cargo:rerun-if-changed=assets/favicon.ico");

    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/favicon.ico");
        res.compile().expect("failed to embed Windows icon");
    }
}
