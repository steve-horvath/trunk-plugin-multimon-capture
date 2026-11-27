# Multimon-ng Decode for Trunk Recorder Pro

A Trunk Recorder Pro plugin that saves selected FM discriminator transmissions
as raw S16LE audio and runs `multimon-ng` once each transmission is complete.

## Captures and decoder output

The plugin subscribes to `tap.fm.discriminator`, the analog FM demodulator output
before de-emphasis and voice filtering. It writes one mono `.raw` file and one
`.jsonl` metadata sidecar per eligible transmission. Audio is signed 16-bit
little-endian PCM at 22,050 Hz. The sidecar contains the stream header and one
record per chunk, including sequence, sample offset/count, frequency, timestamp,
call ID, talkgroup ID, and radio ID.

On a completed stream, the plugin flushes and closes the raw and metadata files,
then invokes multimon-ng once. Standard output is saved in a sibling
`.multimon.txt` file and standard error in `.multimon.log`. If decoding fails,
the raw capture remains intact and the error is logged. An interrupted stream
is flushed at shutdown and not decoded because TRP did not mark it complete.

By default, output goes to `plugin-data/trunk-plugin-multimon-capture/raw/`.
Absolute output paths are used as-is; relative paths resolve beneath the plugin
data directory. Names include system, frequency, timestamp, and stream ID.

## Talkgroup selection and settings

Each system gets TRP's standard Talkgroups selector: every TG, only selected TGs,
or all but selected TGs. The default (no system override) captures all TGs.
Selecting **Only some** with an empty list or entering an invalid ID/range is a
startup configuration error naming the system. The SDK's normal talkgroup list
syntax applies, including numbers, ranges, and patterns.
When a system is in a selected/excluded mode, a tap chunk without a talkgroup ID
is skipped because its eligibility cannot be determined.

Global plugin settings:

| Setting | Description |
| --- | --- |
| `outputDirectory` | Capture destination. Default `raw`; relative paths are under plugin data. |
| `multimonPath` | Optional executable path. Empty searches beside the plugin, then `PATH`. |
| `protocols` | Repeated `-a` protocols. Default: POCSAG512, POCSAG1200, POCSAG2400, FLEX. |
| `repeatableSox` | Pass `-r` to multimon-ng. Default: `true`. |

The Health tab reports completed recordings with page output, recordings with no
page output (only the demodulator header), and decoder errors. A recording counts
as decoded when multimon-ng exits successfully and writes content beyond the
`Enabled demodulators` header. The standard Sent/Failed call counters do not apply
to this live discriminator-tap plugin.

Example `trunk-pro plugin run --settings` JSON:

```json
{
  "config": {
    "outputDirectory": "/home/radio/pocsag/raw",
    "protocols": ["POCSAG512", "POCSAG1200", "POCSAG2400", "FLEX"],
    "repeatableSox": true
  },
  "systems": {
    "halton": {
      "talkgroupMode": "only",
      "talkgroupAllow": ["101", "200-220"]
    }
  }
}
```

`talkgroupMode` accepts `all` (default), `only`, or `allBut`. `talkgroupAllow`
selects inclusions; `talkgroupDeny` selects exclusions. The UI only shows the
list fields relevant to the chosen mode and provides TRP's talkgroup picker.

## Decoder provisioning

TRP has no plugin install hook for system package installation. The release
workflow builds multimon-ng 1.6.1 for each supported platform and places the
matching executable beside the plugin in its archive. No package manager, root
access, or runtime network access is needed for an installed release. For local
development, install multimon-ng yourself or set `multimonPath`. This plugin
never runs a package manager or changes the host system.

## Sandbox build and test (Debian/Ubuntu)

These steps install dependencies in the development sandbox only; they do not
touch a production TRP deployment:

```sh
sudo apt-get install build-essential cmake
git clone --depth 1 --branch 1.6.1 https://github.com/EliasOenal/multimon-ng.git
cmake -S multimon-ng -B multimon-ng/build
cmake --build multimon-ng/build --parallel
sudo cmake --install multimon-ng/build
cargo test --locked
cargo build --release
trunk-pro plugin run ./target/release/trunk-plugin-multimon-capture ~/TrunkRecorderPro --settings examples/settings.json
```

## License

Plugin: GPL-3.0-or-later. multimon-ng is GPL-2.0-or-later; bundled builds must
include its license and source/build provenance as required by its license.
