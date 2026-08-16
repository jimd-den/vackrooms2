//! TEMP-DIAG: validate /tmp/raymarch.frag (assembled by the Python driver)
//! with naga's glsl front end.
fn main() {
    let full = std::fs::read_to_string("/tmp/raymarch.frag").unwrap();
    let mut parser = naga::front::glsl::Frontend::default();
    let options = naga::front::glsl::Options {
        stage: naga::ShaderStage::Fragment,
        defines: Default::default(),
    };
    match parser.parse(&options, &full) {
        Ok(module) => {
            let mut validator = naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            );
            match validator.validate(&module) {
                Ok(_) => println!("VALID"),
                Err(e) => println!("INVALID:\n{}", e.emit_to_string(&full)),
            }
        }
        Err(e) => println!("PARSE ERROR:\n{}", e.emit_to_string(&full)),
    }
}
