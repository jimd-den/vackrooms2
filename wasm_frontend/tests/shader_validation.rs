//! Compiles and validates every WGSL program offline.
//!
//! A shader is only checked when a device creates it, so a syntax or type
//! error surfaces as a lost adapter on whatever machine happens to run the
//! renderer -- and never on a machine with no GPU at all. Running naga's
//! own front end and validator here turns that into an ordinary test
//! failure with a line number.

use naga::valid::{Capabilities, ValidationFlags, Validator};
use wasm_frontend::drivers::webgpu::shader::ShaderProgram;

#[test]
fn every_wgsl_program_parses_and_validates() {
    for program in ShaderProgram::ALL {
        let source = program.source();
        let module = match naga::front::wgsl::parse_str(&source) {
            Ok(module) => module,
            Err(error) => panic!(
                "{program:?} failed to parse:\n{}",
                error.emit_to_string(&source)
            ),
        };
        let mut validator = Validator::new(ValidationFlags::all(), Capabilities::all());
        if let Err(error) = validator.validate(&module) {
            panic!(
                "{program:?} failed validation:\n{}",
                error.emit_to_string(&source)
            );
        }
    }
}
