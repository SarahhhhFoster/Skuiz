//! Run the shared-gain example as a desktop app.
//!
//! Open this alongside the CLAP plugin loaded in a DAW: the two are separate
//! processes, so moving either gain slider drives the other over the bus.

fn main() {
    if let Err(e) =
        skuiz_standalone::run::<shared_gain::SharedGain>(skuiz_standalone::Input::TestTone)
    {
        eprintln!("shared-gain: {e}");
        std::process::exit(1);
    }
}
