//! File source — 0-in / 1-out source that plays back a pre-loaded sample buffer.
//!
//! RT-safe: the buffer is fully decoded at construction (by the application
//! layer); `process` only copies samples and advances the read position — no
//! allocation, locking, or panicking.
//!
//! The buffer uses **planar** layout (matching [`AudioFrame`]):
//! `[ch0_s0, ch0_s1, …, ch1_s0, ch1_s1, …]`. Use [`load_file_source`] to
//! decode a file (FLAC/WAV/PCM via audio-codec-bsd) into a ready-to-play
//! node on a worker thread; the node itself only plays back the resulting
//! `Vec<f32>`.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};

/// 0-in / 1-out source that plays back a pre-loaded sample buffer.
///
/// Created from a fully-decoded planar `Vec<f32>`; `process` is a bounded copy
/// with read-position advancement. Supports optional looping.
pub struct FileSource {
    out_port: [PortDescriptor; 1],
    /// Pre-loaded planar sample buffer (`[ch0…, ch1…]`).
    samples: Vec<f32>,
    channels: u16,
    sample_rate: u32,
    looping: bool,
    /// Current read position in **frames** (0..total_frames).
    read_pos: usize,
    /// Whether playback has finished (non-looping only).
    ended: bool,
}

impl FileSource {
    /// Create from a pre-decoded planar sample buffer.
    ///
    /// - `samples`: planar f32 samples (`[ch0…, ch1…]`).
    /// - `channels`: channel count.
    /// - `sample_rate`: sample rate of the buffer (propagated to output frames).
    /// - `looping`: if `true`, wraps around when the buffer ends.
    ///
    /// An empty buffer (or zero channels) immediately reports `ended`.
    #[must_use]
    pub fn new(samples: Vec<f32>, channels: u16, sample_rate: u32, looping: bool) -> Self {
        let ended = samples.is_empty() || channels == 0;
        Self {
            out_port: [PortDescriptor::output(channels, SampleFormat::F32)],
            samples,
            channels,
            sample_rate,
            looping,
            read_pos: 0,
            ended,
        }
    }

    /// Whether playback has finished (non-looping only; always `false` while
    /// looping, `true` immediately for an empty buffer).
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// Current playback position in frames.
    #[must_use]
    pub fn position(&self) -> usize {
        self.read_pos
    }

    /// Total frames in the buffer (per-channel sample count).
    #[must_use]
    pub fn total_frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }

    /// Decompose into the raw constructor parts — planar samples, channel
    /// count, sample rate, looping flag — so an application-layer buffer
    /// registry can store a decoded file (e.g. from [`load_file_source`])
    /// and rebuild a fresh `FileSource` on every graph (re)build (the node
    /// itself is not `Clone`).
    #[must_use]
    pub fn into_parts(self) -> (Vec<f32>, u16, u32, bool) {
        (self.samples, self.channels, self.sample_rate, self.looping)
    }
}

impl AudioNode for FileSource {
    fn inputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_port
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        _in_frames: &[AudioFrame],
        out_frames: &mut [AudioFrame],
    ) {
        let Some(out) = out_frames.get_mut(0) else {
            return;
        };
        let ch = self.channels as usize;
        let n = out.samples.len();

        out.channels = self.channels;
        out.sample_rate = self.sample_rate;

        if ch == 0 || n == 0 || self.samples.is_empty() || self.ended {
            // Silence: zero channels, empty output, empty buffer, or ended.
            for s in &mut out.samples {
                *s = 0.0;
            }
            return;
        }

        let stride = self.samples.len() / ch; // per-channel frame count
        if stride == 0 {
            // Degenerate: fewer samples than channels.
            for s in &mut out.samples {
                *s = 0.0;
            }
            return;
        }

        let per_ch = n / ch;

        for c in 0..ch {
            let out_offset = c * per_ch;
            let buf_offset = c * stride;
            let out_ch = &mut out.samples[out_offset..out_offset + per_ch];
            for (i, s) in out_ch.iter_mut().enumerate() {
                let local_pos = self.read_pos + i;
                if local_pos < stride {
                    *s = self.samples[buf_offset + local_pos];
                } else if self.looping {
                    *s = self.samples[buf_offset + (local_pos % stride)];
                } else {
                    *s = 0.0;
                }
            }
        }

        // Advance read position.
        if self.looping {
            self.read_pos = (self.read_pos + per_ch) % stride;
        } else {
            self.read_pos += per_ch;
            if self.read_pos >= stride {
                self.ended = true;
            }
        }
    }
}

/// Decode an audio file (FLAC/WAV/PCM via magic-byte sniffing) into a
/// fully-buffered [`FileSource`].
///
/// This is a WORKER-THREAD operation (file I/O + heap allocation) — never
/// call from the RT audio thread. The entire file is decoded up-front into
/// a single planar buffer, so this suits sound-asset playback (samples,
/// loops, IR clips), not multi-gigabyte recordings.
///
/// # Errors
///
/// - I/O, format-recognition, or decode errors from the underlying decoder
///   ([`audio_codec_bsd::CodecError`]).
/// - [`audio_codec_bsd::CodecError::Decode`] if the stream has zero frames
///   (a zero-frame buffer would otherwise play permanent silence).
pub fn load_file_source(
    path: &std::path::Path,
    looping: bool,
) -> Result<FileSource, audio_codec_bsd::CodecError> {
    let mut decoder = audio_codec_bsd::open(path)?;
    let info = decoder.open(path)?;
    let channels = info.channels;
    let sample_rate = info.sample_rate;
    let mut samples: Vec<f32> = Vec::new();
    while let Some(frame) = decoder.next_frame()? {
        samples.extend_from_slice(&frame.samples);
    }
    if samples.is_empty() {
        return Err(audio_codec_bsd::CodecError::Decode(
            "empty stream: zero frames decoded".into(),
        ));
    }
    Ok(FileSource::new(samples, channels, sample_rate, looping))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_mono(node: &mut FileSource, n: usize) -> Vec<f32> {
        let mut out = vec![AudioFrame::from_planar(1, 48_000, vec![0.0; n])];
        let mut ctx = ProcessContext::new(n, 0, 48_000);
        node.process(&mut ctx, &[], &mut out);
        out[0].samples.clone()
    }

    /// Temp-file guard: removes the file on drop, even when an assertion
    /// fails mid-test.
    struct TempWav(std::path::PathBuf);

    impl Drop for TempWav {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Unique temp WAV path (process id + nanosecond timestamp + suffix).
    fn temp_wav_path(suffix: &str) -> TempWav {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut path = std::env::temp_dir();
        path.push(format!(
            "audio_engine_file_source_{}_{suffix}_{nanos}.wav",
            std::process::id()
        ));
        TempWav(path)
    }

    /// Minimal 16-bit PCM RIFF/WAV writer for tests (44-byte header +
    /// samples). `samples_i16` are **interleaved** across `channels`.
    fn write_test_wav(
        path: &std::path::Path,
        channels: u16,
        samples_i16: &[i16],
        sample_rate: u32,
    ) -> std::io::Result<()> {
        let data_len = (samples_i16.len() * 2) as u32;
        let block_align = channels * 2;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_len).to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes()); // byte rate
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&16u16.to_le_bytes()); // bits
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_len.to_le_bytes());
        for s in samples_i16 {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, buf)
    }

    /// 16-bit normalisation used by `WavDecoder`: `sample / 2^15`.
    fn norm16(v: i16) -> f32 {
        f32::from(v) / 32_768.0
    }

    #[test]
    fn plays_buffer() {
        let mut node = FileSource::new(vec![0.1, 0.2, 0.3, 0.4], 1, 48_000, false);
        assert_eq!(node.total_frames(), 4);
        let s = run_mono(&mut node, 256);
        assert_eq!(s.len(), 256);
        assert!((s[0] - 0.1).abs() < 1e-6);
        assert!((s[1] - 0.2).abs() < 1e-6);
        assert!((s[2] - 0.3).abs() < 1e-6);
        assert!((s[3] - 0.4).abs() < 1e-6);
        // Remaining samples are silence.
        for (i, &v) in s.iter().enumerate().skip(4) {
            assert!(v.abs() < 1e-9, "sample {i} should be silent, got {v}");
        }
    }

    #[test]
    fn loops_buffer() {
        let mut node = FileSource::new(vec![0.1, 0.2, 0.3, 0.4], 1, 48_000, true);
        assert!(!node.is_ended());
        let s = run_mono(&mut node, 512);
        assert_eq!(s.len(), 512);
        for (i, &got) in s.iter().enumerate() {
            let expected = match i % 4 {
                0 => 0.1,
                1 => 0.2,
                2 => 0.3,
                _ => 0.4,
            };
            assert!(
                (got - expected).abs() < 1e-6,
                "sample {i}: got {got}, expected {expected}"
            );
        }
        // Looping never ends.
        assert!(!node.is_ended());
    }

    #[test]
    fn empty_buffer_silence() {
        let mut node = FileSource::new(vec![], 1, 48_000, false);
        assert!(node.is_ended(), "empty buffer should report ended");
        assert_eq!(node.total_frames(), 0);
        let s = run_mono(&mut node, 256);
        assert!(s.iter().all(|&v| v.abs() < 1e-9));
    }

    #[test]
    fn stereo_plays_both_channels() {
        // Planar stereo: ch0=[0.1,0.2,0.3,0.4], ch1=[0.5,0.6,0.7,0.8].
        let buf = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        let mut node = FileSource::new(buf, 2, 48_000, false);
        assert_eq!(node.total_frames(), 4);
        // Request 8 output samples = 4 frames x 2 channels.
        let mut out = vec![AudioFrame::from_planar(2, 48_000, vec![0.0; 8])];
        let mut ctx = ProcessContext::new(4, 0, 48_000);
        node.process(&mut ctx, &[], &mut out);
        let s = &out[0].samples;
        // Channel 0.
        assert!((s[0] - 0.1).abs() < 1e-6);
        assert!((s[1] - 0.2).abs() < 1e-6);
        assert!((s[2] - 0.3).abs() < 1e-6);
        assert!((s[3] - 0.4).abs() < 1e-6);
        // Channel 1.
        assert!((s[4] - 0.5).abs() < 1e-6);
        assert!((s[5] - 0.6).abs() < 1e-6);
        assert!((s[6] - 0.7).abs() < 1e-6);
        assert!((s[7] - 0.8).abs() < 1e-6);
        assert_eq!(out[0].channels, 2);
        assert_eq!(out[0].sample_rate, 48_000);
    }

    #[test]
    fn is_ended_after_playback() {
        let mut node = FileSource::new(vec![0.1, 0.2, 0.3, 0.4], 1, 48_000, false);
        assert!(!node.is_ended());
        // Process more samples than the buffer holds.
        let _ = run_mono(&mut node, 256);
        assert!(
            node.is_ended(),
            "should be ended after consuming full buffer"
        );
        // Further processing stays silent.
        let s = run_mono(&mut node, 64);
        assert!(s.iter().all(|&v| v.abs() < 1e-9));
    }

    #[test]
    fn position_tracks_playback() {
        let mut node = FileSource::new(vec![0.5; 100], 1, 48_000, false);
        assert_eq!(node.position(), 0);
        let _ = run_mono(&mut node, 40);
        assert_eq!(node.position(), 40);
        let _ = run_mono(&mut node, 40);
        assert_eq!(node.position(), 80);
        // Consume the remaining frames — position clamps at the buffer end.
        let _ = run_mono(&mut node, 40);
        assert!(node.is_ended());
    }

    #[test]
    fn load_wav_decodes_samples() {
        let tmp = temp_wav_path("mono");
        write_test_wav(&tmp.0, 1, &[1000, 2000, 3000, 4000, -5000], 48_000)
            .expect("write test wav");

        let mut node = load_file_source(&tmp.0, false).expect("decode wav");
        assert_eq!(node.total_frames(), 5);
        assert!(!node.is_ended());

        let s = run_mono(&mut node, 256);
        assert!((s[0] - norm16(1000)).abs() < 1e-6);
        assert!((s[1] - norm16(2000)).abs() < 1e-6);
        assert!((s[2] - norm16(3000)).abs() < 1e-6);
        assert!((s[3] - norm16(4000)).abs() < 1e-6);
        assert!((s[4] - norm16(-5000)).abs() < 1e-6);
        // Tail past the buffer is silence.
        for (i, &v) in s.iter().enumerate().skip(5) {
            assert!(v.abs() < 1e-9, "sample {i} should be silent, got {v}");
        }
        assert!(node.is_ended());
    }

    #[test]
    fn load_wav_stereo_channels() {
        let tmp = temp_wav_path("stereo");
        // Interleaved stereo frames: (ch0, ch1) = (1000, -1000), (2000, -2000).
        write_test_wav(&tmp.0, 2, &[1000, -1000, 2000, -2000], 44_100).expect("write test wav");

        let mut node = load_file_source(&tmp.0, false).expect("decode wav");
        assert_eq!(node.outputs()[0].channels, 2);
        assert_eq!(node.total_frames(), 2);

        // Planar playback: ch0 block first, then ch1 — proves the decoder's
        // de-interleave survived the trip into the node buffer.
        let mut out = vec![AudioFrame::from_planar(2, 44_100, vec![0.0; 4])];
        let mut ctx = ProcessContext::new(2, 0, 44_100);
        node.process(&mut ctx, &[], &mut out);
        let s = &out[0].samples;
        assert!((s[0] - norm16(1000)).abs() < 1e-6);
        assert!((s[1] - norm16(2000)).abs() < 1e-6);
        assert!((s[2] - norm16(-1000)).abs() < 1e-6);
        assert!((s[3] - norm16(-2000)).abs() < 1e-6);
        assert_eq!(out[0].channels, 2);
        assert_eq!(out[0].sample_rate, 44_100);
    }

    #[test]
    fn into_parts_roundtrip() {
        let (samples, channels, sample_rate, looping) =
            FileSource::new(vec![0.1, 0.2, 0.3, 0.4], 1, 48_000, true).into_parts();
        assert_eq!(samples, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(channels, 1);
        assert_eq!(sample_rate, 48_000);
        assert!(looping);
        // Rebuild from the parts — playback still works.
        let mut rebuilt = FileSource::new(samples, channels, sample_rate, looping);
        let s = run_mono(&mut rebuilt, 4);
        assert!((s[0] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn load_missing_file_fails() {
        let path = std::env::temp_dir().join("audio_engine_file_source_no_such_file.wav");
        let Err(err) = load_file_source(&path, false) else {
            panic!("missing file should fail to load");
        };
        assert!(
            matches!(err, audio_codec_bsd::CodecError::Io(_)),
            "expected Io error, got {err:?}"
        );
    }

    #[test]
    fn load_empty_file_fails() {
        let tmp = temp_wav_path("empty");
        std::fs::write(&tmp.0, b"").expect("write empty file");
        let Err(err) = load_file_source(&tmp.0, false) else {
            panic!("empty file should fail to load");
        };
        // 0 bytes → magic-byte sniffing cannot match → Format error.
        assert!(
            matches!(err, audio_codec_bsd::CodecError::Format(_)),
            "expected Format error, got {err:?}"
        );
    }
}
