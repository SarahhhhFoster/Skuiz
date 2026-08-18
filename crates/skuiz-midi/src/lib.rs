//! skuiz-midi: the plugin interface layer — turns messages a DSP produces
//! into MIDI 1.0 bytes on a configurable output.
//!
//! Configuration is deliberately not a parallel system: an output setting is
//! a [`skuiz_core::ParamDef`] with a `choices` list, so it automates, saves
//! with the project, and syncs over IPC exactly like an audio parameter, and
//! hosts render it as a dropdown for free. [`channel_param`] builds the
//! standard channel selector; plugin authors add their own choice params
//! (bit depth, scale, microtuning) the same way.
//!
//! MIDI 1.0 only for now. MPE and MIDI 2.0 UMP are the deferred extension
//! point: both need a wider event than the 3 bytes [`skuiz_core::MidiOut`]
//! carries, so they land together with a wider event type.

use skuiz_core::ParamDef;

/// Channel argument for the message constructors: 0-15 on the wire,
/// displayed to users as 1-16.
pub const MAX_CHANNEL: u8 = 15;

const NOTE_OFF: u8 = 0x80;
const NOTE_ON: u8 = 0x90;
const CONTROL_CHANGE: u8 = 0xB0;
const PITCH_BEND: u8 = 0xE0;

/// Clamp to the 7-bit range MIDI 1.0 data bytes allow.
fn data(v: u8) -> u8 {
    v.min(127)
}

fn status(kind: u8, channel: u8) -> u8 {
    kind | (channel & MAX_CHANNEL)
}

/// Note on. A velocity of 0 is a note off by convention, so callers wanting
/// silence should use [`note_off`] instead.
pub fn note_on(channel: u8, key: u8, velocity: u8) -> [u8; 3] {
    [status(NOTE_ON, channel), data(key), data(velocity)]
}

pub fn note_off(channel: u8, key: u8, velocity: u8) -> [u8; 3] {
    [status(NOTE_OFF, channel), data(key), data(velocity)]
}

pub fn control_change(channel: u8, controller: u8, value: u8) -> [u8; 3] {
    [
        status(CONTROL_CHANGE, channel),
        data(controller),
        data(value),
    ]
}

/// Pitch bend, `-1.0..=1.0`, centred at 0.0 (14-bit, centre 8192).
pub fn pitch_bend(channel: u8, bend: f32) -> [u8; 3] {
    let raw = ((bend.clamp(-1.0, 1.0) as f64 + 1.0) * 8191.5).round() as u16;
    [
        status(PITCH_BEND, channel),
        (raw & 0x7F) as u8,
        ((raw >> 7) & 0x7F) as u8,
    ]
}

/// The 16 MIDI channel labels, as users expect to see them (1-16).
pub const CHANNEL_LABELS: &[&str] = &[
    "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
];

/// A ready-made output-channel dropdown. The parameter's value is the
/// channel index, so pass it straight to the message constructors.
pub const fn channel_param(id: u32) -> ParamDef {
    ParamDef {
        id,
        name: "MIDI Channel",
        min: 0.0,
        max: 15.0,
        default: 0.0,
        choices: CHANNEL_LABELS,
    }
}

/// Read a choice parameter back as a channel index, clamped to 0-15.
pub fn channel_of(value: f64) -> u8 {
    (value.round().clamp(0.0, MAX_CHANNEL as f64)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_encode_status_and_clamp_data() {
        assert_eq!(note_on(0, 60, 100), [0x90, 60, 100]);
        assert_eq!(note_off(3, 60, 0), [0x83, 60, 0]);
        assert_eq!(control_change(15, 7, 64), [0xBF, 7, 64]);
        // channel and data bytes must never corrupt the status nibble
        assert_eq!(note_on(200, 200, 200), [0x98, 127, 127]);
    }

    #[test]
    fn pitch_bend_centres_and_saturates() {
        assert_eq!(pitch_bend(0, 0.0), [0xE0, 0x00, 0x40]); // 8192
        assert_eq!(pitch_bend(0, -1.0), [0xE0, 0x00, 0x00]); // 0
        assert_eq!(pitch_bend(0, 1.0), [0xE0, 0x7F, 0x7F]); // 16383
        assert_eq!(pitch_bend(0, 9.0), pitch_bend(0, 1.0)); // clamped
        for b in [-1.0f32, -0.5, 0.0, 0.25, 1.0] {
            let m = pitch_bend(0, b);
            assert!(m[1] < 128 && m[2] < 128, "data bytes must stay 7-bit");
        }
    }

    #[test]
    fn channel_param_round_trips() {
        let p = channel_param(4);
        assert_eq!((p.low(), p.high()), (0.0, 15.0));
        assert_eq!(p.label(0.0), Some("1"), "channel 0 shows as 1 to users");
        assert_eq!(p.label(15.0), Some("16"));
        assert_eq!(channel_of(15.0), 15);
        assert_eq!(channel_of(99.0), 15, "out-of-range values clamp, not wrap");
        assert_eq!(channel_of(-3.0), 0);
    }
}
