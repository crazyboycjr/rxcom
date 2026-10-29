use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let glslc = std::env::var("GLSLC").unwrap_or_else(|_| "glslc".into());
    for name in ["present.vert", "present.frag"] {
        let src = PathBuf::from("shaders").join(name);
        println!("cargo:rerun-if-changed={}", src.display());
        let status = Command::new(&glslc)
            .args(["-O", "--target-env=vulkan1.3", "-o"])
            .arg(out.join(format!("{name}.spv")))
            .arg(&src)
            .status()
            .unwrap_or_else(|e| panic!("cannot run {glslc} (Arch package shaderc): {e}"));
        assert!(status.success(), "{glslc} failed on {name}");
    }
    println!("cargo:rerun-if-env-changed=GLSLC");
}
