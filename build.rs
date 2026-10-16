use flate2::{Compression, GzBuilder};
use std::{env, fs, io::Write, path::PathBuf};

fn fingerprint(bytes: &[u8]) -> String {
    // Cache identity only, not a cryptographic integrity assertion.
    let hash = bytes.iter().fold(0xcbf29ce484222325_u64, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

fn main() {
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let definitions = [
        ("app.js", "text/javascript; charset=utf-8"),
        ("style.css", "text/css; charset=utf-8"),
        ("vendor/uPlot.iife.min.js", "text/javascript; charset=utf-8"),
        ("vendor/uPlot.min.css", "text/css; charset=utf-8"),
    ];
    println!("cargo:rerun-if-changed=web/index.html");
    let mut html = fs::read_to_string("web/index.html").expect("dashboard HTML");
    let mut assets = Vec::new();
    for (index, (name, kind)) in definitions.into_iter().enumerate() {
        println!("cargo:rerun-if-changed=web/{name}");
        let bytes = fs::read(format!("web/{name}")).expect("dashboard asset");
        let hash = fingerprint(&bytes);
        let (stem, extension) = name.rsplit_once('.').expect("asset extension");
        let route = format!("/assets/{stem}.{hash}.{extension}");
        html = html.replace(&format!("\"/{name}\""), &format!("\"{route}\""));
        assets.push((format!("/{name}"), route, kind, bytes, index));
    }
    assets.push((
        "/".into(),
        "/index.html".into(),
        "text/html; charset=utf-8",
        html.into_bytes(),
        4,
    ));
    let mut generated = String::from("static ASSETS: &[Asset] = &[\n");
    for (alias, route, kind, bytes, index) in assets {
        let hash = fingerprint(&bytes);
        let raw = output.join(format!("asset-{index}"));
        let gzip = output.join(format!("asset-{index}.gz"));
        fs::write(&raw, &bytes).expect("write embedded asset");
        let mut encoder = GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::best());
        encoder.write_all(&bytes).expect("compress asset");
        fs::write(&gzip, encoder.finish().expect("finish compression")).expect("write gzip asset");
        generated.push_str(&format!(
            "Asset {{ alias: {alias:?}, route: {route:?}, kind: {kind:?}, etag: {hash:?}, raw: include_bytes!({raw:?}), gzip: include_bytes!({gzip:?}) }},\n"
        ));
    }
    generated.push_str("];\n");
    fs::write(output.join("embedded_assets.rs"), generated).expect("asset table");
}
