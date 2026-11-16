use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// How static OpenSSL and libtorrent's system dependencies are linked on one target.
struct Linking {
    /// OpenSSL's libraries, as named for `rustc-link-lib`.
    openssl: [&'static str; 2],
    /// The file name a static library called `name` has on disk.
    archive: fn(&str) -> String,
    /// System libraries and frameworks libtorrent and OpenSSL call into.
    system: &'static [&'static str],
}

const UNIX: Linking = Linking { openssl: ["ssl", "crypto"], archive: |name| format!("lib{name}.a"), system: &[] };
const MACOS: Linking = Linking { system: &["framework=CoreFoundation", "framework=SystemConfiguration"], ..UNIX };
// vcpkg names static OpenSSL libssl.lib and libcrypto.lib.
const WINDOWS: Linking = Linking {
    openssl: ["libssl", "libcrypto"],
    archive: |name| format!("{name}.lib"),
    system: &["bcrypt", "mswsock", "ws2_32", "iphlpapi", "crypt32", "advapi32", "user32"],
};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let libtorrent_src = manifest.join("../../vendor/libtorrent");
    if !libtorrent_src.join("deps/try_signal/try_signal.cpp").exists() {
        panic!("vendor/libtorrent is missing or incomplete; run `git submodule update --init --recursive`");
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("cargo sets CARGO_CFG_TARGET_OS");
    let linking = match target_os.as_str() {
        "windows" => WINDOWS,
        "macos" => MACOS,
        _ => UNIX,
    };
    let boost_root = required_env("BOOST_ROOT");
    let openssl_include = required_env("OPENSSL_INCLUDE_DIR");
    let openssl_lib = required_env("OPENSSL_LIB_DIR");
    let [ssl, crypto] = linking.openssl;

    let install = cmake::Config::new(&libtorrent_src)
        .generator("Ninja")
        .profile("Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("CMAKE_INSTALL_LIBDIR", "lib")
        .define("encryption", "ON")
        .define("gnutls", "OFF")
        .define("deprecated-functions", "OFF")
        .define("webtorrent", "OFF")
        .define("Boost_ROOT", &boost_root)
        .define("OPENSSL_INCLUDE_DIR", &openssl_include)
        .define("OPENSSL_SSL_LIBRARY", Path::new(&openssl_lib).join((linking.archive)(ssl)))
        .define("OPENSSL_CRYPTO_LIBRARY", Path::new(&openssl_lib).join((linking.archive)(crypto)))
        .build();

    let defines = exported_defines(&install.join("lib/cmake/LibtorrentRasterbar/LibtorrentRasterbarTargets.cmake"));
    for required in ["TORRENT_USE_OPENSSL", "TORRENT_ABI_VERSION=100", "TORRENT_USE_RTC=0"] {
        assert!(
            defines.iter().any(|d| d == required),
            "libtorrent was configured without {required}; exported defines: {defines:?}"
        );
    }

    let mut shim = cxx_build::bridge("src/bridge.rs");
    shim.file("cpp/shim.cpp")
        .std("c++17")
        .include(install.join("include"))
        .include(Path::new(&boost_root).join("include"))
        .include(&openssl_include);
    for define in &defines {
        match define.split_once('=') {
            Some((name, value)) => shim.define(name, value),
            None => shim.define(define, None),
        };
    }
    if target_os == "windows" {
        // libtorrent sets this for its own sources only, and Boost.Asio's types depend on it.
        shim.define("_WIN32_WINNT", "0x0A00");
        // Without C++ exceptions Boost hides the libtorrent calls that throw.
        shim.flag("/EHsc");
    }
    shim.compile("torrential-shim");

    println!("cargo:rustc-link-search=native={}", install.join("lib").display());
    println!("cargo:rustc-link-lib=static=torrent-rasterbar");
    println!("cargo:rustc-link-search=native={openssl_lib}");
    println!("cargo:rustc-link-lib=static={ssl}");
    println!("cargo:rustc-link-lib=static={crypto}");
    for lib in linking.system {
        println!("cargo:rustc-link-lib={lib}");
    }

    for path in ["cpp/shim.h", "cpp/shim.cpp", "src/bridge.rs"] {
        println!("cargo:rerun-if-changed={path}");
    }
    for path in ["CMakeLists.txt", "include", "src"] {
        println!("cargo:rerun-if-changed={}", libtorrent_src.join(path).display());
    }
    for var in ["BOOST_ROOT", "OPENSSL_INCLUDE_DIR", "OPENSSL_LIB_DIR"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}

fn required_env(name: &str) -> String {
    env::var(name)
        .unwrap_or_else(|_| panic!("{name} is not set; run cargo inside `nix develop` from the workspace root"))
}

/// The shim must use libtorrent's build definitions to preserve ABI compatibility. CMake's export
/// lists them on every platform; MSVC builds get no pkg-config file. Entries that are generator
/// expressions apply only to Debug builds, and libtorrent is built for Release.
fn exported_defines(targets: &Path) -> Vec<String> {
    let text = fs::read_to_string(targets).unwrap_or_else(|e| panic!("reading {}: {e}", targets.display()));
    let list = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("INTERFACE_COMPILE_DEFINITIONS "))
        .unwrap_or_else(|| panic!("{} has no INTERFACE_COMPILE_DEFINITIONS", targets.display()));
    list.trim_matches('"').split(';').filter(|d| !d.contains('$')).map(str::to_owned).collect()
}
