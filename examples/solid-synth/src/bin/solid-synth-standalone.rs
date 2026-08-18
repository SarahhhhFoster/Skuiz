//! Run the SolidJS synth as a desktop app: the fastest way to hear the
//! editor's state turn into sound.

fn main() {
    // The synth generates its own signal, so the shell feeds it silence
    // rather than its test tone.
    if let Err(e) =
        skuiz_standalone::run::<solid_synth::SolidSynth>(skuiz_standalone::Input::Silence)
    {
        eprintln!("solid-synth: {e}");
        std::process::exit(1);
    }
}
