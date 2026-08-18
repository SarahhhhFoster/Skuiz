fn main() {
    println!("cargo:rerun-if-changed=src/envelope.c");
    println!("cargo:rerun-if-changed=src/envelope.h");
    cc::Build::new().file("src/envelope.c").compile("envelope");
}
