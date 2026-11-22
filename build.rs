fn main() {
    // libsbc (BlueZ SBC codec library). Debian: libsbc-dev, Alpine: sbc-dev.
    println!("cargo:rustc-link-lib=sbc");
}
