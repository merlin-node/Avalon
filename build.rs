use std::{env, fs, path::Path};

fn main() {
    let root = Path::new("theme/dist");
    let mut entries = Vec::new();
    fn visit(base: &Path, dir: &Path, output: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("build theme first: npm ci && npm run build in theme/") {
            let path = entry.expect("theme file").path();
            if path.is_dir() { visit(base, &path, output); }
            else {
                let name = path.strip_prefix(base).unwrap().to_str().unwrap().replace('\\', "/");
                output.push(name);
            }
        }
    }
    visit(root, root, &mut entries);
    entries.sort();
    let mut source = String::from("match path.as_str() {\n");
    for name in entries {
        let content_type = if name.ends_with(".svg") { "image/svg+xml" }
            else if name.ends_with(".css") { "text/css; charset=utf-8" }
            else if name.ends_with(".js") { "application/javascript; charset=utf-8" }
            else if name.ends_with(".json") { "application/json" }
            else if name.ends_with(".png") { "image/png" }
            else { "application/octet-stream" };
        source.push_str(&format!(
            "{:?} => Some(({:?}, include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {:?})) as &[u8])),\n",
            name, content_type, format!("/theme/dist/{name}")
        ));
    }
    source.push_str("_ => None,\n}");
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("theme_assets.rs"), source).unwrap();
    println!("cargo:rerun-if-changed=theme/dist");
}
