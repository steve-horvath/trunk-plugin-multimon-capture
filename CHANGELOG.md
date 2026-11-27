# Changelog

## [0.2.0]

- Renamed the plugin display name to Multimon-ng Decode.
- Added per-system TRP Talkgroups selection; unset means all, Only some requires entries, invalid IDs/ranges fail startup.
- Runs bundled or configured multimon-ng once on each completed eligible capture and writes stdout/stderr beside the raw file.
- Release workflow bundles multimon-ng 1.6.1 and its corresponding source archive.
- The macOS universal package is deferred because the GitHub macOS runner did not become available for this release build.
- Preserved per-transmission S16LE capture and JSONL metadata.

## [0.1.0]

- Initial FM discriminator tap capture.
- Saves every `tap.fm.discriminator` stream as mono S16LE PCM at 22,050 Hz.
- Writes one raw file and JSONL metadata sidecar per transmission.
- Keeps capture separate from decoding; no automatic multimon-ng execution.
