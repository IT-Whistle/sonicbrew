//! Audio processing nodes — real DSP `AudioNode` implementations that make
//! sonicbrew function as an actual audio server, not just a signal-passthrough.
//!
//! Every node here honours the [`AudioNode`] RT-safety contract: all state is
//! pre-allocated at construction; `process` does only bounded sample arithmetic
//! with no allocation, locking, or panicking.
//!
//! # Available nodes
//!
//! | Node | Ports | Function |
//! |------|-------|----------|
//! | [`MixerNode`] | N-in / 1-out | Sum N inputs with per-input gain — the core mixing bus |
//! | [`AuxSendNode`] | 1-in / 2-out | Aux send splitter (main passthrough + scaled aux tap) |
//! | [`ChannelMapNode`] | 1-in / 1-out | Channel routing: swap, mute, pan, mono↔stereo |
//! | [`EqNode`] | 1-in / 1-out | Biquad EQ (low/high/band pass, peaking, shelf) |
//! | [`CompressorNode`] | 1-in / 1-out | Dynamic range compression (threshold/ratio/attack/release) |
//! | [`DelayNode`] | 1-in / 1-out | Digital delay line with feedback and wet/dry mix |
//! | [`ChorusNode`] | 1-in / 1-out | LFO-modulated delay chorus effect |
//! | [`FlangerNode`] | 1-in / 1-out | LFO-modulated delay with feedback (flanger) |
//! | [`PhaserNode`] | 1-in / 1-out | LFO-swept allpass cascade phaser |
//! | [`ReverbNode`] | 1-in / 1-out | Schroeder/Freeverb reverb (parallel comb bank + series allpass) |
//! | [`NoiseGateNode`] | 1-in / 1-out | Threshold-based noise gate with attack/hold/release |
//! | [`BitcrusherNode`] | 1-in / 1-out | Bit depth + sample rate reduction (lo-fi/bitcrush) |
//! | [`DistortionNode`] | 1-in / 1-out | Waveshaper distortion (soft clip / hard clip / foldback / overdrive) |
//! | [`LimiterNode`] | 1-in / 1-out | Brick-wall lookahead limiter |
//! | [`MeterNode`] | 1-in / 1-out | Passthrough + RT-safe peak/RMS metering via atomics |
//! | [`TremoloNode`] | 1-in / 1-out | LFO amplitude modulation (tremolo) |
//! | [`NoiseSource`] | 0-in / 1-out | White/pink noise generator (seedable, RT-safe) |
//! | [`ToneGenerator`] | 0-in / 1-out | Multi-waveform oscillator (sine/square/saw/triangle) with phase accumulator |
//! | [`FileSource`] | 0-in / 1-out | Pre-loaded sample buffer playback with looping |
//! | [`StereoWidenerNode`] | 1-in / 1-out | Mid/side stereo width control |

pub mod aux_send;
pub mod bitcrusher;
pub mod channel_map;
pub mod chorus;
pub mod compressor;
pub mod delay;
pub mod distortion;
pub mod eq;
pub mod file_source;
pub mod flanger;
pub mod limiter;
pub mod meter;
pub mod mixer;
pub mod noise_gate;
pub mod noise_source;
pub mod phaser;
pub mod reverb;
pub mod stereo_widener;
pub mod tone_generator;
pub mod tremolo;

pub use aux_send::AuxSendNode;
pub use bitcrusher::BitcrusherNode;
pub use channel_map::{ChannelMapNode, ChannelMode};
pub use chorus::ChorusNode;
pub use compressor::CompressorNode;
pub use delay::DelayNode;
pub use distortion::{DistortionMode, DistortionNode};
pub use eq::{EqNode, FilterType};
pub use file_source::{load_file_source, FileSource};
pub use flanger::FlangerNode;
pub use limiter::LimiterNode;
pub use meter::{Levels, MeterNode};
pub use mixer::MixerNode;
pub use noise_gate::NoiseGateNode;
pub use noise_source::{NoiseColor, NoiseSource};
pub use phaser::PhaserNode;
pub use reverb::ReverbNode;
pub use stereo_widener::StereoWidenerNode;
pub use tone_generator::{ToneGenerator, Waveform};
pub use tremolo::TremoloNode;
