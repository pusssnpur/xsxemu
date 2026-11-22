//! Multi-voice 48 kHz 16-bit Stereo Audio Mixer with RIFF/WAVE export and Windows `winmm` playback.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

pub const OUTPUT_SAMPLE_RATE: u32 = 48_000;
pub const OUTPUT_CHANNELS: u16 = 2;
/// Captured telemetry cap: 2 Mi interleaved samples = 4 MiB (~21 s of 48 kHz stereo).
const MAX_CAPTURE_SAMPLES: usize = 2 * 1024 * 1024;

/// Software stereo mixer backing `XAudio2_9.dll` voices, with optional live Windows playback
/// and deterministic RIFF/WAVE `.wav` export.
pub struct AudioMixer {
    master_volume: f32,
    live_output_enabled: bool,
    /// Interleaved 48 kHz 16-bit stereo PCM frames (`L, R, L, R, ...`).
    mixed_i16: Vec<i16>,
    total_frames_mixed: u64,
    /// Reused per-submit resample buffer (avoids a heap allocation per voice submit).
    scratch: Vec<i16>,
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::new(false)
    }
}

impl AudioMixer {
    pub fn new(live_output_enabled: bool) -> Self {
        Self {
            master_volume: 1.0,
            live_output_enabled,
            mixed_i16: Vec::new(),
            total_frames_mixed: 0,
            scratch: Vec::new(),
        }
    }

    pub fn set_master_volume(&mut self, vol: f32) {
        self.master_volume = vol.clamp(0.0, 4.0);
    }

    pub fn master_volume(&self) -> f32 {
        self.master_volume
    }

    pub fn total_frames_mixed(&self) -> u64 {
        self.total_frames_mixed
    }

    pub fn pcm_byte_len(&self) -> usize {
        self.mixed_i16.len() * 2
    }

    /// Mix a 16-bit PCM voice buffer (mono or stereo, arbitrary sample rate) into the
    /// 48 kHz stereo master stream with linear resampling, volume scaling, and soft clipping.
    pub fn submit_pcm16(
        &mut self,
        raw_bytes: &[u8],
        src_channels: u16,
        src_sample_rate: u32,
        voice_volume: f32,
    ) -> u64 {
        let ch = src_channels.clamp(1, 2) as usize;
        let bytes_per_frame = ch * 2;
        if raw_bytes.len() < bytes_per_frame {
            return 0;
        }
        let in_frames = raw_bytes.len() / bytes_per_frame;
        let in_rate = src_sample_rate.max(8_000) as f64;
        let ratio = in_rate / (OUTPUT_SAMPLE_RATE as f64);
        let out_frames = ((in_frames as f64) / ratio).round().max(1.0) as usize;

        // Fast exit: nothing will observe the samples (no live output, capture buffer full).
        let capture_has_room = self.mixed_i16.len() + out_frames * 2 <= MAX_CAPTURE_SAMPLES;
        if !self.live_output_enabled && !capture_has_room {
            self.total_frames_mixed += out_frames as u64;
            return out_frames as u64;
        }

        let gain = (self.master_volume * voice_volume.clamp(0.0, 4.0)) / 32768.0;
        let mut chunk_out = std::mem::take(&mut self.scratch);
        chunk_out.clear();
        chunk_out.reserve(out_frames * 2);

        if src_sample_rate == OUTPUT_SAMPLE_RATE && out_frames == in_frames {
            // 48 kHz pass-through: no interpolation needed.
            for frame in raw_bytes[..in_frames * bytes_per_frame].chunks_exact(bytes_per_frame) {
                let l = i16::from_le_bytes([frame[0], frame[1]]);
                let r = if ch == 2 { i16::from_le_bytes([frame[2], frame[3]]) } else { l };
                chunk_out.push(f32_to_i16_soft_clip(l as f32 * gain));
                chunk_out.push(f32_to_i16_soft_clip(r as f32 * gain));
            }
        } else {
            let last = in_frames - 1;
            let mut src_pos = 0.0_f64;
            for _ in 0..out_frames {
                let idx0 = (src_pos as usize).min(last);
                let idx1 = (idx0 + 1).min(last);
                let frac = (src_pos - idx0 as f64).clamp(0.0, 1.0) as f32;
                src_pos += ratio;

                let (l0, r0) = read_frame_i16(raw_bytes, idx0, ch);
                let (l1, r1) = read_frame_i16(raw_bytes, idx1, ch);
                let l = ((l0 as f32) + ((l1 as f32) - (l0 as f32)) * frac) * gain;
                let r = ((r0 as f32) + ((r1 as f32) - (r0 as f32)) * frac) * gain;

                chunk_out.push(f32_to_i16_soft_clip(l));
                chunk_out.push(f32_to_i16_soft_clip(r));
            }
        }

        if self.live_output_enabled {
            play_pcm16_live(&chunk_out);
        }

        // Cap captured telemetry buffer to 4 MiB (~21 seconds of 48 kHz stereo)
        if capture_has_room {
            self.mixed_i16.extend_from_slice(&chunk_out);
        }
        self.scratch = chunk_out;
        self.total_frames_mixed += out_frames as u64;
        out_frames as u64
    }

    /// Write the mixed 48 kHz 16-bit stereo PCM buffer to a RIFF/WAVE (`.wav`) file.
    pub fn write_wav(&self, path: &Path) -> io::Result<()> {
        let data_bytes = (self.mixed_i16.len() * 2) as u32;
        let riff_size = 36u32.saturating_add(data_bytes);
        let byte_rate = OUTPUT_SAMPLE_RATE * (OUTPUT_CHANNELS as u32) * 2;
        let block_align = OUTPUT_CHANNELS * 2;

        let mut f = File::create(path)?;
        f.write_all(b"RIFF")?;
        f.write_all(&riff_size.to_le_bytes())?;
        f.write_all(b"WAVE")?;
        // fmt chunk
        f.write_all(b"fmt ")?;
        f.write_all(&16u32.to_le_bytes())?;
        f.write_all(&1u16.to_le_bytes())?; // PCM
        f.write_all(&OUTPUT_CHANNELS.to_le_bytes())?;
        f.write_all(&OUTPUT_SAMPLE_RATE.to_le_bytes())?;
        f.write_all(&byte_rate.to_le_bytes())?;
        f.write_all(&block_align.to_le_bytes())?;
        f.write_all(&16u16.to_le_bytes())?; // bits per sample
        // data chunk
        f.write_all(b"data")?;
        f.write_all(&data_bytes.to_le_bytes())?;
        let mut raw = Vec::with_capacity(self.mixed_i16.len() * 2);
        for &s in &self.mixed_i16 {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        f.write_all(&raw)
    }
}

#[inline]
fn read_frame_i16(buf: &[u8], frame_idx: usize, channels: usize) -> (i16, i16) {
    let off = frame_idx * channels * 2;
    let l = i16::from_le_bytes([buf[off], buf[off + 1]]);
    let r = if channels == 2 {
        i16::from_le_bytes([buf[off + 2], buf[off + 3]])
    } else {
        l
    };
    (l, r)
}

#[inline]
fn f32_to_i16_soft_clip(x: f32) -> i16 {
    // Smooth saturation avoids harsh digital clipping when mixing multiple voices
    let s = if x.abs() > 0.95 { x.tanh() } else { x };
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Optional real-time Windows `winmm.dll` (`waveOut`) playback for live sound on Windows PCs.
#[cfg(target_os = "windows")]
fn play_pcm16_live(samples: &[i16]) {
    use core::ffi::c_void;
    use std::ptr;
    use xsx_abi::services::WaveFormatEx;

    #[repr(C)]
    struct WaveHdr {
        lp_data: *mut u8,
        dw_buffer_length: u32,
        dw_bytes_recorded: u32,
        dw_user: usize,
        dw_flags: u32,
        dw_loops: u32,
        lp_next: *mut c_void,
        reserved: usize,
    }

    #[link(name = "winmm")]
    extern "system" {
        fn waveOutOpen(
            phwo: *mut *mut c_void,
            u_device_id: u32,
            pwfx: *const WaveFormatEx,
            dw_callback: usize,
            dw_instance: usize,
            fdw_open: u32,
        ) -> u32;
        fn waveOutPrepareHeader(hwo: *mut c_void, pwh: *mut WaveHdr, cbwh: u32) -> u32;
        fn waveOutWrite(hwo: *mut c_void, pwh: *mut WaveHdr, cbwh: u32) -> u32;
        fn waveOutUnprepareHeader(hwo: *mut c_void, pwh: *mut WaveHdr, cbwh: u32) -> u32;
        fn waveOutClose(hwo: *mut c_void) -> u32;
    }

    if samples.is_empty() {
        return;
    }
    let fmt = WaveFormatEx {
        w_format_tag: 1,
        n_channels: OUTPUT_CHANNELS,
        n_samples_per_sec: OUTPUT_SAMPLE_RATE,
        n_avg_bytes_per_sec: OUTPUT_SAMPLE_RATE * (OUTPUT_CHANNELS as u32) * 2,
        n_block_align: OUTPUT_CHANNELS * 2,
        w_bits_per_sample: 16,
        cb_size: 0,
    };
    unsafe {
        let mut hwo: *mut c_void = ptr::null_mut();
        // WAVE_MAPPER = 0xFFFF_FFFF
        if waveOutOpen(&mut hwo, 0xFFFF_FFFF, &fmt, 0, 0, 0) != 0 || hwo.is_null() {
            return;
        }
        let mut bytes: Vec<u8> = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let mut hdr = WaveHdr {
            lp_data: bytes.as_mut_ptr(),
            dw_buffer_length: bytes.len() as u32,
            dw_bytes_recorded: 0,
            dw_user: 0,
            dw_flags: 0,
            dw_loops: 0,
            lp_next: ptr::null_mut(),
            reserved: 0,
        };
        let hdr_sz = core::mem::size_of::<WaveHdr>() as u32;
        if waveOutPrepareHeader(hwo, &mut hdr, hdr_sz) == 0 {
            let _ = waveOutWrite(hwo, &mut hdr, hdr_sz);
            // Wait briefly for short buffers to drain (capped at 100ms)
            let dur_ms = ((samples.len() / 2) as u64 * 1000 / OUTPUT_SAMPLE_RATE as u64).min(100);
            std::thread::sleep(std::time::Duration::from_millis(dur_ms.max(5)));
            let _ = waveOutUnprepareHeader(hwo, &mut hdr, hdr_sz);
        }
        let _ = waveOutClose(hwo);
    }
}

#[cfg(not(target_os = "windows"))]
fn play_pcm16_live(_samples: &[i16]) {}

/// Generate a 440 Hz A4 stereo 16-bit sine wave PCM buffer for clean-room audio tests.
pub fn generate_sine_pcm16(freq_hz: f32, sample_rate: u32, frames: usize, amplitude: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let t = (i as f32) / (sample_rate as f32);
        let sample = ((2.0 * core::f32::consts::PI * freq_hz * t).sin() * amplitude * 32767.0)
            .clamp(-32767.0, 32767.0) as i16;
        let bytes = sample.to_le_bytes();
        out.extend_from_slice(&bytes); // L
        out.extend_from_slice(&bytes); // R
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_mixes_and_writes_valid_wav() {
        let mut mixer = AudioMixer::new(false);
        let pcm_24k = generate_sine_pcm16(440.0, 24_000, 120, 0.5);
        // 120 frames @ 24kHz -> 240 frames @ 48kHz
        let mixed = mixer.submit_pcm16(&pcm_24k, 2, 24_000, 1.0);
        assert_eq!(mixed, 240);
        assert_eq!(mixer.total_frames_mixed(), 240);

        let wav_path = std::env::temp_dir().join(format!("xsxemu_test_{}.wav", std::process::id()));
        mixer.write_wav(&wav_path).unwrap();
        let wav = std::fs::read(&wav_path).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + 240 * 4);
        let _ = std::fs::remove_file(&wav_path);
    }

    #[test]
    fn passthrough_48k_and_capture_cap_fast_paths() {
        let mut mixer = AudioMixer::new(false);
        let pcm = generate_sine_pcm16(1000.0, 48_000, 480, 0.25);
        assert_eq!(mixer.submit_pcm16(&pcm, 2, 48_000, 1.0), 480);
        assert_eq!(mixer.pcm_byte_len(), 480 * 4);
        // Passthrough at unity gain reproduces the input samples (within 1 LSB of 32767/32768 scaling).
        let first_in = i16::from_le_bytes([pcm[40], pcm[41]]);
        assert!((mixer.mixed_i16[20] as i32 - first_in as i32).abs() <= 1);
        // Fill the capture buffer; further submits only advance the frame counter.
        let big = vec![0u8; (MAX_CAPTURE_SAMPLES - mixer.mixed_i16.len()) * 2];
        mixer.submit_pcm16(&big, 2, 48_000, 1.0);
        let len_before = mixer.pcm_byte_len();
        assert_eq!(mixer.submit_pcm16(&pcm, 2, 48_000, 1.0), 480);
        assert_eq!(mixer.pcm_byte_len(), len_before);
    }
}
