#[cfg(feature = "cpp")]
fn main() {
    let mut b = cc::Build::new();
    b.cpp(true)
        .flag("-std=c++11")
        .file("src/esaxx.cpp")
        .include("src");

    // ort-sys links against the dynamic CRT on Windows; esaxx must match
    // or the linker errors with LNK2038 RuntimeLibrary mismatch.
    #[cfg(not(target_os = "windows"))]
    b.static_crt(true);

    #[cfg(target_os = "macos")]
    b.flag("-stdlib=libc++");

    b.compile("esaxx");
}

#[cfg(not(feature = "cpp"))]
fn main() {}