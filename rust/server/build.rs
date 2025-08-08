fn main() {
    // Rebuild if proto files change
    println!("cargo:rerun-if-changed=proto/fpn.proto");

    prost_build::Config::new()
        .out_dir(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"))
        // Allow future type/field attributes if needed
        .compile_protos(&["proto/fpn.proto"], &["proto"]) // proto files, include dirs
        .expect("failed to compile protobufs with prost-build");
}
