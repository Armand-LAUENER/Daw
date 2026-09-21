//! Notes arriving from another thread, the way a MIDI port delivers them.
//!
//! A keyboard's callback runs on a thread the MIDI backend owns — not the
//! audio thread, not the control thread — and writes straight into the synth's
//! queue. That crossing is the part of live input that belongs to this engine;
//! opening the port belongs to the backend. So it is tested here, where no
//! hardware is needed to do it, rather than left to a keyboard nobody has
//! plugged in.
//!
//! The queue takes one producer. These tests use exactly one, because a second
//! is the bug they would otherwise be hiding.

use std::sync::mpsc;
use std::thread;

use engine_core::{AudioBuffer, Node, NoteEvent, Synth, note_channel};
use engine_dsp::{Adsr, Waveform};

const SR: f32 = 48_000.0;
const FRAMES: usize = 480;

fn synth(queue: engine_core::NoteQueue) -> Synth {
    // Open at once and held, so a rendered block either sounds or does not.
    let envelope = Adsr::new(0.0, 0.0, 1.0, 0.0, SR);
    Synth::new(8, Waveform::Saw, &envelope, queue, SR)
}

fn render(synth: &mut Synth, at: u64) -> Vec<f32> {
    let mut data = vec![0.0; FRAMES];
    synth.set_frame(at);
    synth.process(&mut AudioBuffer::new(&mut data, 1));
    data
}

#[test]
fn a_note_sent_from_another_thread_sounds() {
    let (mut sender, queue) = note_channel(64);
    let mut synth = synth(queue);
    let (done, wait) = mpsc::channel();

    // The only writer, as a port's callback is.
    thread::spawn(move || {
        sender.send(NoteEvent::on(60, 1.0, 0)).expect("the queue was empty");
        done.send(()).expect("the test is still listening");
    });

    wait.recv().expect("the sending thread finished");
    let out = render(&mut synth, 0);
    assert_eq!(synth.active_voices(), 1, "the note never reached the synth");
    assert!(out.iter().any(|s| *s != 0.0), "the note reached the synth but made no sound");
}

#[test]
fn a_note_arriving_mid_run_is_picked_up_by_a_later_block() {
    // What actually happens at a keyboard: the key goes down while the audio
    // thread is somewhere in the middle of a block. It cannot be heard in one
    // already rendered, but it must not be lost either.
    let (mut sender, queue) = note_channel(64);
    let mut synth = synth(queue);
    let (done, wait) = mpsc::channel();

    let silent = render(&mut synth, 0);
    assert!(silent.iter().all(|s| *s == 0.0), "something sounded before any note was sent");

    thread::spawn(move || {
        sender.send(NoteEvent::on(64, 1.0, 0)).expect("the queue was empty");
        done.send(()).expect("the test is still listening");
    });
    wait.recv().expect("the sending thread finished");

    let out = render(&mut synth, FRAMES as u64);
    assert!(out.iter().any(|s| *s != 0.0), "the note was dropped rather than played late");
}

#[test]
fn a_held_note_keeps_sounding_until_the_key_comes_up() {
    let (mut sender, queue) = note_channel(64);
    let mut synth = synth(queue);
    let (done, wait) = mpsc::channel();

    thread::spawn(move || {
        sender.send(NoteEvent::on(60, 1.0, 0)).expect("the queue was empty");
        done.send(sender).expect("the test is still listening");
    });
    let mut sender = wait.recv().expect("the sending thread handed the queue back");

    for block in 0..8 {
        let out = render(&mut synth, block * FRAMES as u64);
        assert!(out.iter().any(|s| *s != 0.0), "the note stopped on its own in block {block}");
    }

    sender.send(NoteEvent::off(60, 8 * FRAMES as u64)).expect("the queue has room");
    render(&mut synth, 8 * FRAMES as u64);
    assert_eq!(synth.active_voices(), 0, "the key came up and the note carried on");
}

#[test]
fn a_full_queue_refuses_rather_than_blocking() {
    // The port's callback must never wait on the audio thread. When the queue
    // is full the send fails, which is what lets the caller count what was lost
    // instead of stalling a thread it does not own.
    let (mut sender, _queue) = note_channel(2);
    assert!(sender.send(NoteEvent::on(60, 1.0, 0)).is_ok());
    assert!(sender.send(NoteEvent::on(61, 1.0, 0)).is_ok());
    assert!(sender.is_full());
    assert!(sender.send(NoteEvent::on(62, 1.0, 0)).is_err(), "a full queue accepted another note");
}

#[test]
fn a_burst_of_keys_all_arrive() {
    // A hand landing on a chord sends several messages in a row, from the same
    // thread, faster than any block boundary.
    let (mut sender, queue) = note_channel(64);
    let mut synth = synth(queue);
    let (done, wait) = mpsc::channel();

    thread::spawn(move || {
        for pitch in [60u8, 64, 67, 71] {
            sender.send(NoteEvent::on(pitch, 0.8, 0)).expect("the queue was empty");
        }
        done.send(()).expect("the test is still listening");
    });
    wait.recv().expect("the sending thread finished");

    render(&mut synth, 0);
    assert_eq!(synth.active_voices(), 4, "part of the chord went missing");
}
