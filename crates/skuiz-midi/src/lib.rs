//! skuiz-midi: the plugin interface layer — turns messages a DSP produces
//! into MIDI events on a configurable output.
//!
//! Configuration is deliberately not a parallel system: an output setting is
//! a [`skuiz_core::ParamDef`] with a `choices` list, so it automates, saves
//! with the project, and syncs over IPC exactly like an audio parameter, and
//! hosts render it as a dropdown for free. [`channel_param`] builds the
//! standard channel selector; plugin authors add their own choice params
//! (bit depth, scale, microtuning) the same way.
//!
//! Events are UMP words ([`MidiEvent`]), so MIDI 1.0 and MIDI 2.0 both fit:
//! the 1.0 constructors pack their bytes into one UMP word, and the `*_2`
//! constructors emit MIDI 2.0 channel voice (16-bit velocity). Adapters
//! hand MIDI 1.0 events to hosts as native MIDI 1.0, so a MIDI-1.0-only
//! plugin behaves exactly as before. MPE needs no special casing here —
//! per-note messages just use per-note channels.

#![warn(missing_docs)]
use skuiz_core::{MidiEvent, ParamDef};

/// Channel argument for the message constructors: 0-15 on the wire,
/// displayed to users as 1-16.
pub const MAX_CHANNEL: u8 = 15;

const NOTE_OFF: u8 = 0x80;
const NOTE_ON: u8 = 0x90;
const CONTROL_CHANGE: u8 = 0xB0;
const PITCH_BEND: u8 = 0xE0;

// MIDI 2.0 channel voice keeps the same status nibbles, one nibble higher
// in the word (message type 0x4, two words).
const MIDI2_NOTE_OFF: u32 = 0x8 << 20;
const MIDI2_NOTE_ON: u32 = 0x9 << 20;

/// Clamp to the 7-bit range MIDI 1.0 data bytes allow.
fn data(v: u8) -> u8 {
    v.min(127)
}

// Out-of-range channels wrap here but clamp in `channel_of` — deliberate:
// the constructors take a raw u8 from DSP code, where a bad channel is a
// bug and the mask just keeps the status byte legal, while `channel_of`
// reads a user-automatable value, where clamping is the kinder behaviour.
fn status(kind: u8, channel: u8) -> u8 {
    kind | (channel & MAX_CHANNEL)
}

/// Note on. A velocity of 0 is a note off by convention, so callers wanting
/// silence should use [`note_off`] instead.
pub fn note_on(channel: u8, key: u8, velocity: u8) -> MidiEvent {
    MidiEvent::from_midi1([status(NOTE_ON, channel), data(key), data(velocity)])
}

/// Note off. `velocity` is release velocity, which most hosts ignore.
pub fn note_off(channel: u8, key: u8, velocity: u8) -> MidiEvent {
    MidiEvent::from_midi1([status(NOTE_OFF, channel), data(key), data(velocity)])
}

/// Control change (CC). `controller` is the CC number, e.g. 7 for volume.
pub fn control_change(channel: u8, controller: u8, value: u8) -> MidiEvent {
    MidiEvent::from_midi1([
        status(CONTROL_CHANGE, channel),
        data(controller),
        data(value),
    ])
}

/// Pitch bend, `-1.0..=1.0`, centred at 0.0 (14-bit, centre 8192).
pub fn pitch_bend(channel: u8, bend: f32) -> MidiEvent {
    let raw = ((bend.clamp(-1.0, 1.0) as f64 + 1.0) * 8191.5).round() as u16;
    MidiEvent::from_midi1([
        status(PITCH_BEND, channel),
        (raw & 0x7F) as u8,
        ((raw >> 7) & 0x7F) as u8,
    ])
}

/// MIDI 2.0 note on: 16-bit velocity (clamped to 1 — velocity 0 is not a
/// legal MIDI 2.0 note on), no per-note attribute. Whether the host sees
/// UMP depends on the adapter; CLAP carries it as a MIDI2 event.
pub fn note_on2(channel: u8, key: u8, velocity: u16) -> MidiEvent {
    note2(MIDI2_NOTE_ON, channel, key, velocity.max(1))
}

/// MIDI 2.0 note off: 16-bit release velocity.
pub fn note_off2(channel: u8, key: u8, velocity: u16) -> MidiEvent {
    note2(MIDI2_NOTE_OFF, channel, key, velocity)
}

fn note2(status: u32, channel: u8, key: u8, velocity: u16) -> MidiEvent {
    MidiEvent::from_ump(&[
        0x4000_0000 | status | ((channel & MAX_CHANNEL) as u32) << 16 | (data(key) as u32) << 8,
        (velocity as u32) << 16,
    ])
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
        shared: true,
    }
}

/// Read a choice parameter back as a channel index, clamped to 0-15
/// (unlike the message constructors, which wrap — see `status`).
pub fn channel_of(value: f64) -> u8 {
    (value.round().clamp(0.0, MAX_CHANNEL as f64)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_encode_status_and_clamp_data() {
        assert_eq!(note_on(0, 60, 100).midi1_bytes(), Some([0x90, 60, 100]));
        assert_eq!(note_off(3, 60, 0).midi1_bytes(), Some([0x83, 60, 0]));
        assert_eq!(control_change(15, 7, 64).midi1_bytes(), Some([0xBF, 7, 64]));
        // channel and data bytes must never corrupt the status nibble
        assert_eq!(note_on(200, 200, 200).midi1_bytes(), Some([0x98, 127, 127]));
    }

    #[test]
    fn pitch_bend_centres_and_saturates() {
        assert_eq!(pitch_bend(0, 0.0).midi1_bytes(), Some([0xE0, 0x00, 0x40])); // 8192
        assert_eq!(pitch_bend(0, -1.0).midi1_bytes(), Some([0xE0, 0x00, 0x00])); // 0
        assert_eq!(pitch_bend(0, 1.0).midi1_bytes(), Some([0xE0, 0x7F, 0x7F])); // 16383
        assert_eq!(pitch_bend(0, 9.0), pitch_bend(0, 1.0)); // clamped
        for b in [-1.0f32, -0.5, 0.0, 0.25, 1.0] {
            let m = pitch_bend(0, b).midi1_bytes().unwrap();
            assert!(m[1] < 128 && m[2] < 128, "data bytes must stay 7-bit");
        }
    }

    #[test]
    fn midi2_notes_are_two_word_ump_with_16_bit_velocity() {
        // Message type 0x4 (MIDI 2.0 channel voice), status 0x9, ch 1,
        // key 60, attribute 0; velocity 0xF800 in the high half of word 1.
        assert_eq!(note_on2(1, 60, 0xF800).words(), &[0x4091_3C00, 0xF800_0000]);
        assert_eq!(note_off2(1, 60, 0).words(), &[0x4081_3C00, 0]);
        // Not reducible to MIDI 1.0 bytes.
        assert_eq!(note_on2(1, 60, 0xF800).midi1_bytes(), None);
        // Velocity 0 is not a legal MIDI 2.0 note on; clamped to 1.
        assert_eq!(note_on2(1, 60, 0).words()[1], 0x0001_0000);
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
