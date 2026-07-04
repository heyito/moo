fn main() {
    println!("cargo:rustc-link-search=native=/opt/homebrew/opt/libkrun/lib");
    println!("cargo:rustc-link-lib=dylib=krun");
}
