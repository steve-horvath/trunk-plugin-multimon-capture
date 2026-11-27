//! Captures every FM discriminator tap as raw S16LE PCM, one file per transmission.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::PathBuf,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use trunk_recorder_plugin::{Host, Manifest, NoConfig, Plugin, Setup, Tap, TapChunk, TapData};

const SAMPLE_RATE: u32 = 22_050;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
struct Config {
    /// Output directory
    ///
    /// Where captured transmissions are written. An absolute path is used as-is;
    /// a relative path is resolved under this plugin's persistent data directory.
    /// The default is the plugin's own data directory/raw.
    output_directory: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { output_directory: "raw".into() }
    }
}

struct CapturedStream {
    raw: BufWriter<File>,
    metadata: BufWriter<File>,
    raw_path: PathBuf,
}

struct MultimonCapture {
    host: Host,
    output_dir: PathBuf,
    streams: HashMap<u64, CapturedStream>,
}

impl MultimonCapture {
    fn open_stream(&self, chunk: &TapChunk) -> io::Result<CapturedStream> {
        let system = safe_component(&chunk.short_name);
        let frequency = chunk.freq_hz.round().max(0.0) as u64;
        let timestamp_ms = (chunk.time * 1000.0).max(0.0) as u64;
        let stem = format!("{system}_{frequency}Hz_{timestamp_ms}_stream{}", chunk.stream);

        // create_new avoids overwriting a previous capture if the recorder restarts
        // within the same millisecond or a stream ID is reused.
        for suffix in 0..1000_u32 {
            let name = if suffix == 0 { stem.clone() } else { format!("{stem}_{suffix}") };
            let raw_path = self.output_dir.join(format!("{name}.raw"));
            let metadata_path = self.output_dir.join(format!("{name}.jsonl"));

            let raw = match OpenOptions::new().write(true).create_new(true).open(&raw_path) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            };

            let metadata = match OpenOptions::new().write(true).create_new(true).open(&metadata_path) {
                Ok(file) => file,
                Err(e) => {
                    let _ = fs::remove_file(&raw_path);
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        continue;
                    }
                    return Err(e);
                }
            };

            let mut captured = CapturedStream { raw: BufWriter::new(raw), metadata: BufWriter::new(metadata), raw_path };
            let header = json!({
                "event": "stream_start",
                "tap": "fm.discriminator",
                "stream": chunk.stream,
                "system": chunk.short_name,
                "system_id": chunk.system_id,
                "system_index": chunk.system,
                "frequency_hz": chunk.freq_hz,
                "slot": chunk.slot,
                "sample_rate": SAMPLE_RATE,
                "sample_format": "s16le",
                "channels": 1,
                "started_at": chunk.time,
            });
            write_json_line(&mut captured.metadata, &header)?;
            return Ok(captured);
        }

        Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not find an unused capture filename"))
    }

    fn write_chunk(&mut self, chunk: TapChunk) {
        if chunk.first {
            if let Some(previous) = self.streams.remove(&chunk.stream) {
                let _ = finish_stream(previous);
            }
            match self.open_stream(&chunk) {
                Ok(stream) => {
                    self.host.info(format!("Capturing {} Hz from {} to {}", chunk.freq_hz.round(), chunk.short_name, stream.raw_path.display()));
                    self.streams.insert(chunk.stream, stream);
                }
                Err(e) => {
                    self.host.error(format!("Could not create capture for stream {}: {e}", chunk.stream));
                    return;
                }
            }
        }

        let result = match self.streams.get_mut(&chunk.stream) {
            Some(stream) => write_chunk_files(stream, &chunk),
            None => {
                self.host.warn(format!("Ignoring chunk {} for stream {} because its first chunk was not received", chunk.seq, chunk.stream));
                return;
            }
        };

        if let Err(e) = result {
            self.host.error(format!("Writing stream {} failed: {e}", chunk.stream));
            if let Some(stream) = self.streams.remove(&chunk.stream) {
                let _ = finish_stream(stream);
            }
            return;
        }

        if chunk.last
            && let Some(stream) = self.streams.remove(&chunk.stream)
        {
            match finish_stream(stream) {
                Ok(path) => self.host.info(format!("Finished discriminator capture: {}", path.display())),
                Err(e) => self.host.error(format!("Could not finish stream: {e}")),
            }
        }
    }
}

fn write_chunk_files(stream: &mut CapturedStream, chunk: &TapChunk) -> io::Result<()> {
    let (rate, samples) = match &chunk.data {
        TapData::Pcm { rate, samples } => (*rate, samples),
        other => {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("expected PCM samples, received {other:?}")));
        }
    };

    if rate != SAMPLE_RATE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("expected {SAMPLE_RATE} Hz PCM, received {rate} Hz")));
    }

    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    stream.raw.write_all(&bytes)?;

    let record = json!({
        "event": "chunk",
        "stream": chunk.stream,
        "sequence": chunk.seq,
        "first": chunk.first,
        "last": chunk.last,
        "sample_offset": chunk.offset,
        "sample_count": samples.len(),
        "sample_rate": rate,
        "time": chunk.time,
        "system": chunk.short_name,
        "system_id": chunk.system_id,
        "system_index": chunk.system,
        "frequency_hz": chunk.freq_hz,
        "slot": chunk.slot,
        "call_id": chunk.call_id,
        "talkgroup": chunk.talkgroup,
        "unit": chunk.unit,
        "voice_mode": chunk.voice_mode,
    });
    write_json_line(&mut stream.metadata, &record)
}

fn write_json_line(writer: &mut impl Write, value: &serde_json::Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(io::Error::other)?;
    writer.write_all(b"\n")
}

fn finish_stream(mut stream: CapturedStream) -> io::Result<PathBuf> {
    stream.raw.flush()?;
    stream.metadata.flush()?;
    Ok(stream.raw_path)
}

fn safe_component(value: &str) -> String {
    let cleaned: String = value.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if cleaned.is_empty() { "unknown-system".into() } else { cleaned }
}

impl Plugin for MultimonCapture {
    type Config = Config;
    type SystemConfig = NoConfig;

    fn manifest() -> Manifest {
        Manifest { name: "Multimon Capture".into(), subscribe: vec!["tap.fm.discriminator".into()], ..trunk_recorder_plugin::manifest!() }
    }

    fn start(host: Host, setup: Setup<Config, NoConfig>) -> Result<Self, String> {
        let configured = PathBuf::from(setup.config.output_directory.trim());
        let output_dir = if configured.is_absolute() { configured } else { setup.data_dir.join(configured) };

        fs::create_dir_all(&output_dir).map_err(|e| format!("could not create output directory {}: {e}", output_dir.display()))?;

        host.info(format!("Saving all FM discriminator streams as S16LE mono, {SAMPLE_RATE} Hz in {}", output_dir.display()));

        Ok(Self { host, output_dir, streams: HashMap::new() })
    }

    fn tap(&mut self, chunk: TapChunk) {
        if chunk.tap == Tap::FmDiscriminator {
            self.write_chunk(chunk);
        }
    }

    fn shutdown(&mut self, _grace: std::time::Duration) {
        for (id, stream) in self.streams.drain() {
            match finish_stream(stream) {
                Ok(path) => self.host.info(format!("Closed unfinished stream {id}: {}", path.display())),
                Err(e) => self.host.error(format!("Could not close stream {id}: {e}")),
            }
        }
    }
}

fn main() {
    trunk_recorder_plugin::run::<MultimonCapture>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_safe_and_nonempty() {
        assert_eq!(safe_component("Halton Fire/EMS"), "Halton_Fire_EMS");
        assert_eq!(safe_component(""), "unknown-system");
        assert_eq!(safe_component("site_1-west"), "site_1-west");
    }
}
