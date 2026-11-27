//! Captures selected FM discriminator taps and decodes completed streams with multimon-ng.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use trunk_recorder_plugin::{Host, Manifest, Metrics, Plugin, Setup, TalkgroupFilter, Talkgroups, Tap, TapChunk, TapData};

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
    /// multimon-ng executable
    ///
    /// Path to multimon-ng. When empty, the plugin searches beside itself and then PATH.
    multimon_path: String,
    /// Decoder protocols
    ///
    /// Protocol names passed as repeated -a arguments. Defaults to POCSAG512, POCSAG1200, POCSAG2400 and FLEX.
    protocols: Vec<String>,
    /// Use multimon-ng's repeatable SoX mode
    ///
    /// Passes `-r` to multimon-ng. Enabled by default.
    repeatable_sox: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output_directory: "raw".into(),
            multimon_path: String::new(),
            protocols: vec!["POCSAG512".into(), "POCSAG1200".into(), "POCSAG2400".into(), "FLEX".into()],
            repeatable_sox: true,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
struct SystemConfig {
    /// Talkgroups to capture and decode
    ///
    /// Choose every talkgroup, only selected talkgroups, or all but selected talkgroups.
    #[serde(flatten)]
    talkgroups: Talkgroups,
}

struct CapturedStream {
    raw: BufWriter<File>,
    metadata: BufWriter<File>,
    raw_path: PathBuf,
    talkgroup_unresolved: bool,
}

struct MultimonCapture {
    host: Host,
    output_dir: PathBuf,
    streams: HashMap<u64, CapturedStream>,
    filters: HashMap<String, TalkgroupFilter>,
    decoder: Option<PathBuf>,
    protocols: Vec<String>,
    repeatable_sox: bool,
    decode_stats: DecodeStats,
    last_metrics_at: Option<Instant>,
    ignored_streams: HashSet<u64>,
}

#[derive(Default)]
struct DecodeStats {
    recordings_with_pages: u64,
    recordings_without_pages: u64,
    decoder_errors: u64,
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

            let mut captured = CapturedStream { raw: BufWriter::new(raw), metadata: BufWriter::new(metadata), raw_path, talkgroup_unresolved: false };
            let header = json!({
                "event": "stream_start",
                "tap": "fm.discriminator",
                "stream": chunk.stream,
                "system": chunk.short_name,
                "system_id": chunk.system_id,
                "system_index": chunk.system,
                "talkgroup": chunk.talkgroup,
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
        let restricted = talkgroup_requires_resolution(self.filters.get(&chunk.short_name), None);
        if chunk.first {
            self.ignored_streams.remove(&chunk.stream);
            if chunk.talkgroup.is_some_and(|tg| !talkgroup_passes(self.filters.get(&chunk.short_name), Some(tg))) {
                if !chunk.last {
                    self.ignored_streams.insert(chunk.stream);
                }
                return;
            }
        } else if self.ignored_streams.contains(&chunk.stream) {
            if chunk.last {
                self.ignored_streams.remove(&chunk.stream);
            }
            return;
        }
        if chunk.first {
            if let Some(previous) = self.streams.remove(&chunk.stream) {
                let _ = finish_stream(previous);
            }
            match self.open_stream(&chunk) {
                Ok(mut stream) => {
                    stream.talkgroup_unresolved = restricted && chunk.talkgroup.is_none();
                    self.host.info(format!("Capturing {} Hz from {} to {}", chunk.freq_hz.round(), chunk.short_name, stream.raw_path.display()));
                    self.streams.insert(chunk.stream, stream);
                }
                Err(e) => {
                    self.host.error(format!("Could not create capture for stream {}: {e}", chunk.stream));
                    return;
                }
            }
        }

        // TRP can hand out discriminator samples before a conventional call has
        // acquired its filed talkgroup. Keep those initial samples provisionally;
        // decide the stream when its first TGID arrives, or discard it at end if
        // the TGID never becomes known.
        if restricted && let Some(tg) = chunk.talkgroup {
            if !talkgroup_passes(self.filters.get(&chunk.short_name), Some(tg)) {
                if let Some(stream) = self.streams.remove(&chunk.stream) {
                    discard_stream(stream);
                }
                if !chunk.last {
                    self.ignored_streams.insert(chunk.stream);
                }
                return;
            }
            if let Some(stream) = self.streams.get_mut(&chunk.stream) {
                stream.talkgroup_unresolved = false;
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
            if stream.talkgroup_unresolved {
                discard_stream(stream);
                self.host.info(format!("Discarded discriminator stream {} because its talkgroup was never identified", chunk.stream));
                return;
            }
            match finish_stream(stream) {
                Ok(path) => {
                    self.host.info(format!("Finished discriminator capture: {}", path.display()));
                    self.decode(&path);
                }
                Err(e) => self.host.error(format!("Could not finish stream: {e}")),
            }
        }
    }

    fn decode(&mut self, raw_path: &std::path::Path) {
        let Some(executable) = &self.decoder else {
            self.host.error("multimon-ng was not found; raw capture was saved without decoding");
            self.decode_stats.decoder_errors += 1;
            self.report_metrics(false);
            return;
        };
        let output_path = raw_path.with_extension("multimon.txt");
        let error_path = raw_path.with_extension("multimon.log");
        let protocols = self.protocols.clone();
        let repeatable_sox = self.repeatable_sox;
        let result = (|| -> io::Result<bool> {
            let output = decoder_command(executable, raw_path, &protocols, repeatable_sox).output()?;
            let has_page = has_decoded_payload(&output.stdout);
            fs::write(&output_path, output.stdout)?;
            fs::write(&error_path, &output.stderr)?;
            if !output.status.success() {
                return Err(io::Error::other(format!("multimon-ng exited with {}", output.status)));
            }
            Ok(has_page)
        })();
        match result {
            Ok(true) => {
                self.decode_stats.recordings_with_pages += 1;
                self.host.info(format!("Decoded capture to {}", output_path.display()));
            }
            Ok(false) => {
                self.decode_stats.recordings_without_pages += 1;
                self.host.info(format!("No page output in {}", output_path.display()));
            }
            Err(e) => {
                self.decode_stats.decoder_errors += 1;
                self.host.error(format!("Could not decode {}: {e}", raw_path.display()));
            }
        }
        self.report_metrics(false);
    }

    fn report_metrics(&mut self, force: bool) {
        let now = Instant::now();
        if !force && self.last_metrics_at.is_some_and(|last| now.duration_since(last) < Duration::from_secs(2)) {
            return;
        }
        self.host.metrics(&Metrics {
            extra: BTreeMap::from([
                ("Decoded recordings".into(), json!(self.decode_stats.recordings_with_pages)),
                ("No page output".into(), json!(self.decode_stats.recordings_without_pages)),
                ("Decoder errors".into(), json!(self.decode_stats.decoder_errors)),
            ]),
            ..Metrics::default()
        });
        self.last_metrics_at = Some(now);
    }
}

fn decoder_command(executable: &std::path::Path, raw_path: &std::path::Path, protocols: &[String], repeatable_sox: bool) -> Command {
    let mut command = Command::new(executable);
    if repeatable_sox {
        command.arg("-r");
    }
    command.arg("-t").arg("raw").arg("-f").arg("alpha");
    for protocol in protocols {
        command.arg("-a").arg(protocol);
    }
    command.arg(raw_path);
    command
}

fn has_decoded_payload(output: &[u8]) -> bool {
    String::from_utf8_lossy(output).lines().any(|line| {
        let line = line.trim_matches(|c: char| c.is_whitespace() || c == '\u{4}');
        !line.is_empty() && !line.starts_with("Enabled demodulators:")
    })
}

fn talkgroup_passes(filter: Option<&TalkgroupFilter>, talkgroup: Option<u32>) -> bool {
    filter.is_none_or(|f| f.is_empty() || talkgroup.is_some_and(|tg| f.passes(tg)))
}

fn talkgroup_requires_resolution(filter: Option<&TalkgroupFilter>, talkgroup: Option<u32>) -> bool {
    talkgroup.is_none() && filter.is_some_and(|f| !f.is_empty())
}

fn find_decoder(configured: &str) -> Option<PathBuf> {
    if !configured.trim().is_empty() {
        let path = PathBuf::from(configured.trim());
        return path.is_file().then_some(path);
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
    {
        for name in ["multimon-ng", "multimon-ng.exe"] {
            let path = parent.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    #[cfg(windows)]
    let names = ["multimon-ng.exe"];
    #[cfg(not(windows))]
    let names = ["multimon-ng"];
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).flat_map(|dir| names.iter().map(move |name| dir.join(name)).collect::<Vec<_>>()).find(|path| path.is_file())
    })
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

fn discard_stream(stream: CapturedStream) {
    let raw_path = stream.raw_path.clone();
    let metadata_path = raw_path.with_extension("jsonl");
    drop(stream);
    let _ = fs::remove_file(raw_path);
    let _ = fs::remove_file(metadata_path);
}

fn safe_component(value: &str) -> String {
    let cleaned: String = value.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if cleaned.is_empty() { "unknown-system".into() } else { cleaned }
}

impl Plugin for MultimonCapture {
    type Config = Config;
    type SystemConfig = SystemConfig;

    fn manifest() -> Manifest {
        Manifest { name: "Multimon-ng Decode".into(), subscribe: vec!["tap.fm.discriminator".into()], ..trunk_recorder_plugin::manifest!() }
    }

    fn start(host: Host, setup: Setup<Config, SystemConfig>) -> Result<Self, String> {
        if setup.config.protocols.is_empty()
            || setup.config.protocols.iter().any(|p| p.trim().is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            return Err("protocols must contain one or more multimon-ng mode names (letters, digits, underscores)".into());
        }
        let configured = PathBuf::from(setup.config.output_directory.trim());
        let output_dir = if configured.is_absolute() { configured } else { setup.data_dir.join(configured) };

        fs::create_dir_all(&output_dir).map_err(|e| format!("could not create output directory {}: {e}", output_dir.display()))?;

        let mut filters = HashMap::new();
        for system in &setup.systems {
            let config = system.config.as_ref().cloned().unwrap_or_default();
            filters.insert(system.short_name.clone(), config.talkgroups.filter_for(&system.short_name)?);
        }
        let decoder = find_decoder(&setup.config.multimon_path);
        if decoder.is_none() {
            host.warn("multimon-ng not found; raw captures will continue without decoding");
        }
        host.info(format!("Saving selected FM discriminator streams as S16LE mono, {SAMPLE_RATE} Hz in {}", output_dir.display()));

        Ok(Self {
            host,
            output_dir,
            streams: HashMap::new(),
            filters,
            decoder,
            protocols: setup.config.protocols,
            repeatable_sox: setup.config.repeatable_sox,
            decode_stats: DecodeStats::default(),
            last_metrics_at: None,
            ignored_streams: HashSet::new(),
        })
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
        self.report_metrics(true);
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

    #[test]
    fn repeatable_sox_defaults_on_and_can_be_disabled() {
        assert!(Config::default().repeatable_sox);
        let disabled: Config = serde_json::from_value(json!({ "repeatableSox": false })).unwrap();
        assert!(!disabled.repeatable_sox);
    }

    #[test]
    fn decoder_command_adds_repeatable_flag_only_when_enabled() {
        let executable = PathBuf::from("multimon-ng");
        let raw_path = PathBuf::from("capture.raw");
        let protocols = vec!["POCSAG512".to_string(), "FLEX".to_string()];
        let args =
            |enabled| decoder_command(&executable, &raw_path, &protocols, enabled).get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(args(true), ["-r", "-t", "raw", "-f", "alpha", "-a", "POCSAG512", "-a", "FLEX", "capture.raw"]);
        assert_eq!(args(false), ["-t", "raw", "-f", "alpha", "-a", "POCSAG512", "-a", "FLEX", "capture.raw"]);
    }

    #[test]
    fn decode_output_requires_payload_beyond_the_header() {
        assert!(!has_decoded_payload(b"Enabled demodulators: POCSAG512 FLEX\n"));
        assert!(has_decoded_payload(b"Enabled demodulators: POCSAG512 FLEX\nPOCSAG512: Address: 160113 Alpha: TEST\x04\n"));
        assert!(!has_decoded_payload(b"\n  Enabled demodulators: POCSAG512 FLEX \x04\n"));
    }

    #[test]
    fn talkgroup_modes_have_safe_empty_and_invalid_behavior() {
        let all: Talkgroups = serde_json::from_value(json!({})).unwrap();
        assert!(all.filter().unwrap().passes(101));

        let only: Talkgroups = serde_json::from_value(json!({ "talkgroupMode": "only", "talkgroupAllow": ["100-110"] })).unwrap();
        let filter = only.filter().unwrap();
        assert!(filter.passes(101));
        assert!(!filter.passes(111));
        assert!(!talkgroup_passes(Some(&filter), None), "an unknown TGID is not yet eligible");
        assert!(talkgroup_requires_resolution(Some(&filter), None), "unknown TGID should defer the decision until later chunks");
        assert!(!talkgroup_requires_resolution(Some(&filter), Some(101)));
        assert!(talkgroup_passes(Some(&Talkgroups::default().filter().unwrap()), None));

        let empty_only: Talkgroups = serde_json::from_value(json!({ "talkgroupMode": "only" })).unwrap();
        assert!(empty_only.filter().is_err());

        let invalid: Talkgroups = serde_json::from_value(json!({ "talkgroupMode": "allBut", "talkgroupDeny": ["not-a-tgid"] })).unwrap();
        assert!(invalid.filter().is_err());
    }
}
