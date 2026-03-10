fn main() {
    // Runtime dispatch arrives with the engine; for now the binary
    // exists so packaging and CI lanes have something to build.
    eprintln!(
        "agentd {}: workflow runtime scaffolding",
        env!("CARGO_PKG_VERSION")
    );
}
