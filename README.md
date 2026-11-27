# Multimon Capture for Trunk Recorder Pro

A Trunk Recorder Pro plugin that captures every FM discriminator signal tap as
raw audio, one file per transmission. It does not run a decoder: the files are
ready to feed to `multimon-ng` or another pager/data decoder separately.

## What it captures

The plugin subscribes to `tap.fm.discriminator`. This is the analog FM
demodulator output before de-emphasis and voice filtering:

- **Format:** signed 16-bit little-endian PCM (`s16le`)
- **Channels:** mono
- **Sample rate:** 22,050 Hz
- **Scope:** every discriminator stream delivered by Trunk Recorder Pro,
  including streams that aren't associated with a recorded call
- **Files:** one `.raw` file and one `.jsonl` metadata sidecar per transmission

The capture is not filtered by talkgroup, call recording rules, or whether a
call WAV is saved. A stream ends when the recorder marks its tap stream as
finished. An unfinished stream is flushed when the plugin shuts down.

## Output

By default, files are written under the plugin's persistent data directory:

```
plugin-data/trunk-plugin-multimon-capture/raw/
  system_857262500Hz_1795700000000_stream123.raw
  system_857262500Hz_1795700000000_stream123.jsonl
```

The raw file contains only little-endian signed 16-bit samples. It has no WAV
header. The JSONL sidecar contains a stream header followed by one record per
chunk, including sequence, sample offset/count, frequency, timestamp, and any
call/talkgroup metadata known for that chunk.

## Settings

| Setting | Description |
|---|---|
| **Output directory** | Destination for captures. An absolute path is used directly. A relative path is resolved under the plugin's persistent data directory. Default: `raw`. |

Example settings:

```json
{
  "config": {
    "outputDirectory": "/home/steve/HALTON/pocsag/raw"
  }
}
```

Ensure the Trunk Recorder Pro process has write permission to the directory.

## Using the raw files

For example, decode a captured stream with:

```sh
multimon-ng -t raw -a POCSAG512 -a POCSAG1200 -a POCSAG2400 \
  -f alpha capture.raw
```

The decoder configuration and supported baud rates can be adjusted independently
of capture. This first version deliberately only captures raw audio; automatic
decoding, filtering, and forwarding are out of scope.

## Building

```sh
cargo build --release
trunk-pro plugin run ./target/release/trunk-plugin-multimon-capture ~/TrunkRecorderPro
```

## License

GPL-3.0-or-later, like Trunk Recorder.
