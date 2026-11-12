//! Embeds the web app in the binary. `TORRENTIAL_WEB_DIST` names a built copy of it; without that,
//! this builds `web/` with bun.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCES: [&str; 6] = ["src", "index.html", "package.json", "bun.lock", "vite.config.ts", "tsconfig.json"];

fn main() {
    println!("cargo:rerun-if-env-changed=TORRENTIAL_WEB_DIST");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let dist = match std::env::var_os("TORRENTIAL_WEB_DIST").filter(|dir| !dir.is_empty()) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            println!("cargo:rerun-if-changed={}", dir.display());
            dir
        }
        None => build(&out.join("web")),
    };
    let dist = dist.canonicalize().unwrap_or_else(|e| panic!("cannot read the web app at {}: {e}", dist.display()));
    assert!(dist.join("index.html").is_file(), "{} has no index.html, so it is not a built web app", dist.display());
    let mut files = Vec::new();
    collect(&dist, &dist, &mut files);
    files.sort();
    let mut table = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (route, path) in files {
        writeln!(table, "    ({route:?}, include_bytes!({:?})),", path.display().to_string()).expect("strings grow");
    }
    table.push_str("];\n");
    std::fs::write(out.join("web_assets.rs"), table).expect("OUT_DIR is writable");
}

/// Builds `web/` into `dist` with bun.
fn build(dist: &Path) -> PathBuf {
    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web");
    for source in SOURCES {
        println!("cargo:rerun-if-changed={}", web.join(source).display());
    }
    let bun = |args: &[&str]| {
        let status = Command::new("bun").args(args).current_dir(&web).status().unwrap_or_else(|e| {
            panic!(
                "cannot run bun to build the web app: {e}. Run cargo inside `nix develop`, or set \
                 TORRENTIAL_WEB_DIST to a built copy of web/"
            )
        });
        assert!(status.success(), "`bun {}` failed in {}", args.join(" "), web.display());
    };
    bun(&["install", "--frozen-lockfile"]);
    let dist_arg = dist.display().to_string();
    bun(&["run", "build", "--outDir", &dist_arg, "--emptyOutDir"]);
    dist.to_path_buf()
}

/// Every file under `dir` as its URL path and its path on disk.
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("the directory lists").path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            let relative = path.strip_prefix(root).expect("the file is under the root");
            let route: Vec<_> = relative.components().map(|c| c.as_os_str().to_string_lossy()).collect();
            files.push((format!("/{}", route.join("/")), path));
        }
    }
}
