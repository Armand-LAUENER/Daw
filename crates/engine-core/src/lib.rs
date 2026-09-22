//! The audio graph: buffers, nodes, and the mix that joins them.
//!
//! Everything here runs on the audio thread, under the same rule as the rest of
//! the engine: no allocation, no locking, no I/O. Buffers are borrowed rather
//! than owned and nodes work in place, so a signal crossing the graph is never
//! copied for the sake of the plumbing.
//!
//! Audio is laid out **planar** — one channel after another — not interleaved.
//! A graph moves whole channels around, so interleaving would make every access
//! strided; the device's interleaved shape is converted once at each end.

mod automation;
mod buffer;
mod eq;
mod event;
mod graph;
mod limiter;
mod node;
mod note;
mod sequencer;
mod swap;
mod synth;
mod transport;

pub use automation::{AutomationQueue, AutomationSender, ParamEvent, ParamId, automation_channel};
pub use buffer::AudioBuffer;
pub use eq::{BandSettings, Eq};
pub use event::{EventQueue, EventSender, Scheduled, event_channel};
pub use graph::{Graph, GraphBuilder, GraphError, NodeId};
pub use limiter::{LIMITER_LOOKAHEAD_MS, LIMITER_RELEASE_MS, Limiter};
pub use node::{Gain, Mute, Node, PARAM_RAMP_MS, Pan, Solo, SoloBus};
pub use note::{Note, NoteAction, NoteEvent, NoteQueue, NoteSender, PianoRoll, note_channel, pitch_to_hz};
pub use sequencer::Sequencer;
pub use swap::{GraphPlayer, GraphSwapper, SWAP_FADE_MS, graph_channel};
pub use synth::Synth;
pub use transport::{Clock, TRANSPORT_FADE_MS, Transport, TransportCommand, TransportControl, transport_channel};
