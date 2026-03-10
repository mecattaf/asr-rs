use anyhow::{Context, Result};
use audioadapter_buffers::direct::InterleavedSlice;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
use rubato::{Fft, FixedSync, Resampler};
use std::sync::{Arc, Mutex};

const TARGET_SAMPLE_RATE: usize = 16_000;
const RING_BUFFER_CAPACITY: usize = 512 * 1024; // ~32s at 16kHz
const RESAMPLER_CHUNK_SIZE: usize = 1024;
const RESAMPLER_SUB_CHUNKS: usize = 2;

struct ResamplerState {
    resampler: Fft<f32>,
    input_buffer: Vec<f32>,
    output_chunk_buffer: Vec<f32>,
    chunk_size: usize,
    output_frames_max: usize,
}

impl ResamplerState {
    /// Process a single chunk through the resampler.
    /// Returns the number of output frames written to output_chunk_buffer.
    fn process_chunk(&mut self, chunk: &[f32]) -> Option<usize> {
        let input_adapter =
            InterleavedSlice::new(chunk, 1, chunk.len()).ok()?;
        let mut output_adapter =
            InterleavedSlice::new_mut(&mut self.output_chunk_buffer, 1, self.output_frames_max)
                .ok()?;
        let (_read, written) = self
            .resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, None)
            .ok()?;
        Some(written)
    }
}

/// Mono downmix: average all channels.
fn mono_mix(samples: &[f32], channels: u16) -> Vec<f32> {
    samples
        .chunks(usize::from(channels))
        .map(|frame| frame.iter().sum::<f32>() / f32::from(channels))
        .collect()
}

#[allow(deprecated)] // cpal 0.17.3 deprecates .name() in favor of .description()
fn device_name_str(device: &cpal::Device) -> String {
    device.name().unwrap_or_else(|_| "<unknown>".into())
}

/// Start audio capture. Returns (cpal::Stream, ring buffer consumer).
/// The Stream must be kept alive (not dropped) for capture to continue.
/// The consumer is Send and can be moved to another task.
pub fn start_capture(device_name: &str) -> Result<(cpal::Stream, HeapCons<f32>)> {
    let host = cpal::default_host();

    let device = if device_name == "default" {
        host.default_input_device()
            .context("no default input device")?
    } else {
        host.input_devices()
            .context("failed to enumerate input devices")?
            .find(|d| {
                device_name_str(d).contains(device_name)
            })
            .with_context(|| format!("input device {device_name:?} not found"))?
    };

    let supported = device
        .default_input_config()
        .context("no default input config")?;
    let sample_rate = supported.sample_rate() as usize;
    let channels = supported.channels();
    let sample_format = supported.sample_format();

    tracing::info!(
        "audio device: {:?}, rate={sample_rate}, ch={channels}, fmt={sample_format:?}",
        device_name_str(&device)
    );

    let config = cpal::StreamConfig {
        channels,
        sample_rate: sample_rate as u32,
        buffer_size: cpal::BufferSize::Default,
    };

    // Ring buffer: carries 16kHz mono f32
    let rb = HeapRb::<f32>::new(RING_BUFFER_CAPACITY);
    let (producer, consumer) = rb.split();

    // Resampler: source rate -> 16kHz mono
    let needs_resample = sample_rate != TARGET_SAMPLE_RATE || channels > 1;

    let resampler_state = if needs_resample {
        let resampler = Fft::<f32>::new(
            sample_rate,
            TARGET_SAMPLE_RATE,
            RESAMPLER_CHUNK_SIZE,
            RESAMPLER_SUB_CHUNKS,
            1, // mono (downmix before resampling)
            FixedSync::Input,
        )
        .context("failed to create resampler")?;

        let chunk_size = resampler.input_frames_max();
        let output_frames_max = resampler.output_frames_max();

        Some(Arc::new(Mutex::new(ResamplerState {
            resampler,
            input_buffer: Vec::with_capacity(chunk_size * 2),
            output_chunk_buffer: vec![0.0; output_frames_max],
            chunk_size,
            output_frames_max,
        })))
    } else {
        None
    };

    let stream = build_stream(
        &device,
        &config,
        sample_format,
        channels,
        resampler_state,
        producer,
    )?;

    stream.play().context("failed to start audio stream")?;

    Ok((stream, consumer))
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    channels: u16,
    resampler_state: Option<Arc<Mutex<ResamplerState>>>,
    producer: HeapProd<f32>,
) -> Result<cpal::Stream> {
    match sample_format {
        cpal::SampleFormat::F32 => {
            build_stream_typed::<f32>(device, config, channels, resampler_state, producer)
        }
        cpal::SampleFormat::I16 => {
            build_stream_typed::<i16>(device, config, channels, resampler_state, producer)
        }
        cpal::SampleFormat::U16 => {
            build_stream_typed::<u16>(device, config, channels, resampler_state, producer)
        }
        fmt => anyhow::bail!("unsupported sample format: {fmt:?}"),
    }
}

fn build_stream_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: u16,
    resampler_state: Option<Arc<Mutex<ResamplerState>>>,
    mut producer: HeapProd<f32>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    // Pre-allocated scratch buffers captured by the closure
    let mut scratch = Vec::<f32>::new();

    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                // Convert to f32
                scratch.clear();
                scratch.reserve(data.len());
                for &sample in data {
                    scratch.push(cpal::Sample::from_sample(sample));
                }

                let mono = if channels > 1 {
                    mono_mix(&scratch, channels)
                } else {
                    scratch.clone()
                };

                if let Some(ref rs) = resampler_state {
                    let mut guard = match rs.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };

                    guard.input_buffer.extend_from_slice(&mono);
                    let chunk_size = guard.chunk_size;

                    // Process full chunks
                    while guard.input_buffer.len() >= chunk_size {
                        let chunk: Vec<f32> =
                            guard.input_buffer.drain(..chunk_size).collect();

                        if let Some(written) = guard.process_chunk(&chunk) {
                            // Copy output before pushing (avoids borrow conflict)
                            let output: Vec<f32> =
                                guard.output_chunk_buffer[..written].to_vec();
                            producer.push_slice(&output);
                        }
                    }
                } else {
                    // Already 16kHz mono — push directly
                    producer.push_slice(&mono);
                }
            },
            |err| {
                eprintln!("[asr-rs] audio stream error: {err}");
            },
            None,
        )
        .context("failed to build input stream")?;

    Ok(stream)
}

/// Drain the ring buffer consumer and convert f32 samples to s16le bytes.
pub fn drain_s16le(consumer: &mut HeapCons<f32>) -> Vec<u8> {
    let available = consumer.occupied_len();
    if available == 0 {
        return Vec::new();
    }
    let mut samples = vec![0.0f32; available];
    let popped = consumer.pop_slice(&mut samples);
    samples.truncate(popped);

    let mut buf = Vec::with_capacity(popped * 2);
    for &sample in &samples {
        let clamped = sample.clamp(-1.0, 1.0);
        #[allow(clippy::cast_possible_truncation)]
        let pcm16 = (clamped * f32::from(i16::MAX)) as i16;
        buf.extend_from_slice(&pcm16.to_le_bytes());
    }
    buf
}
