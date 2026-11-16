fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("app.ico");
        res.set("ProductName", "R2GFK SunDEC");
        res.set("FileDescription", "R2GFK SunDEC — DMR декодер для SunSDR");
        if let Err(e) = res.compile() {
            println!("cargo:warning=Не удалось вшить иконку: {e}");
        }
        println!("cargo:rerun-if-changed=app.ico");
    }
}