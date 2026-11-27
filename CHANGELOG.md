# Changelog

## [0.1.0]

- Initial FM discriminator tap capture.
- Saves every `tap.fm.discriminator` stream as mono S16LE PCM at 22,050 Hz.
- Writes one raw file and JSONL metadata sidecar per transmission.
- Keeps capture separate from decoding; no automatic multimon-ng execution.
