# CL5 remote-control protocol notes

This is the intentionally small protocol surface used by CL5 Monitor Mix. The
fake console implements the rows below; it is a test double only and never
discovers or connects to a real desk.

## Wire format

Each TCP message ends in `\n`. Clients use `get` and `set`; a console reports
out-of-band changes with `NOTIFY`. This is confirmed by Yamaha's [PC Remote
Control Protocol Specification v1.02 rev.3](https://mx.yamaha.com/files/download/other_assets/4/1307804/pc_remote_control_protocol_spec_v102_rev3_en.pdf)
and the [Yamaha RCP notes](https://github.com/BrenekH/yamaha-rcp-docs#basics).
The fake's `OK` and `NOTIFY set` response shape follows the latter's documented
examples and must be checked against the physical CL5 during bridge work.

| Value | Read | Set | range / status | source |
| --- | --- | --- | --- | --- |
| Send level | `get MIXER:Current/InCh/ToMix/Level <input-0-based> <mix-0-based>` | `set MIXER:Current/InCh/ToMix/Level <input> <mix> <value>` | `-13800..1000`; `-32768` is -infinity | [RCP notes: level](https://github.com/BrenekH/yamaha-rcp-docs/blob/master/commands/current_inch_tomix_level.md); generic dB range in [RCP notes](https://github.com/BrenekH/yamaha-rcp-docs#data-types) |
| Send on/off | `get MIXER:Current/InCh/ToMix/On <input> <mix>` | `set MIXER:Current/InCh/ToMix/On <input> <mix> <0-or-1>` | `0` false, `1` true | [RCP notes: on](https://github.com/BrenekH/yamaha-rcp-docs/blob/master/commands/current_inch_tomix_on.md) |
| Send pan | `get MIXER:Current/InCh/ToMix/Pan <input> <mix>` | `set MIXER:Current/InCh/ToMix/Pan <input> <mix> <value>` | integer -63..63; stereo role required (physical-console check pending) | [RCP notes: pan](https://github.com/BrenekH/yamaha-rcp-docs/blob/master/commands/current_inch_tomix_pan.md) |
| Input name | `get MIXER:Current/InCh/Label/Name <input> 0` | `set MIXER:Current/InCh/Label/Name <input> 0 "name"` | quoted string | [RCP notes: name](https://github.com/BrenekH/yamaha-rcp-docs/blob/master/commands/current_inch_label_name.md) |
| Input color | `get MIXER:Current/InCh/Label/Color <input> 0` | `set MIXER:Current/InCh/Label/Color <input> 0 <color>` | named quoted string (e.g. `"Blue"`); see color readback below | [RCP notes: color](https://github.com/BrenekH/yamaha-rcp-docs/blob/master/commands/current_inch_label_color.md) |
| Mix name | `get MIXER:Current/Mix/Label/Name <mix> 0` | `set MIXER:Current/Mix/Label/Name <mix> 0 "name"` | quoted string | [RCP notes: mix name](https://github.com/BrenekH/yamaha-rcp-docs/blob/master/commands/current_mix_label_name.md) |
| Stereo-linked mix | `get MIXER:Current/Mix/Role <mix> 0` | n/a | named role: StereoL/StereoR enable pan; Mono/unknown do not | [CL/QL parameter capture](https://github.com/bitfocus/companion-module-yamaha-rcp/blob/master/schemas/CLQL%20Parameters-1.txt) |

For deterministic tests, the fake initializes names as the quoted empty string
and retains its legacy `BusType` default of `0`. This is a fake-console
convention, not a stereo flag or a claim about CL5 encoding. The bridge uses
Role for stereo detection, never BusType.

CL5 capacity used by the fake: 72 mono inputs, 8 stereo inputs, and 24 mixes.
The count is **unconfirmed — check on the real console**; it is deliberately
not presented as an official protocol fact.

## Scene state

`sscurrent_ex MIXER:Lib/Scene` asks for the current scene number, and
`ssinfo_ex MIXER:Lib/Scene <number>` obtains its name. `ssrecall_ex
MIXER:Lib/Scene <number>` is the recall command. The command forms are sourced
from the CL5-capable [Bitfocus Yamaha RCP module](https://github.com/bitfocus/companion-module-yamaha-rcp/blob/master/src/variables.ts).
The exact CL5 replies and notification ordering are **unconfirmed — check on
the real console**. The fake announces a recall with `NOTIFY ssrecall_ex ...`
and the name with `NOTIFY ssinfo_ex ...`; it also announces changed send values.

For a scene store, the expected event shape is `NOTIFY ssstore_ex
MIXER:Lib/Scene <number>`. Its exact command name, arguments, whether the scene
name is emitted separately, and ordering are **unconfirmed — check on the real
console**. This provisional shape follows the Yamaha protocol's command/event
model in the [PC Remote Control Protocol Specification v1.02
rev.3](https://mx.yamaha.com/files/download/other_assets/4/1307804/pc_remote_control_protocol_spec_v102_rev3_en.pdf);
the CL5-capable Bitfocus module above is the source for the `MIXER:Lib/Scene`
scene path. The fake does not implement storing scenes because it only needs to
model recalls for tests.

The app must only observe scene changes; it never recalls or stores a scene on
a real console.

## Channel color readback

The [CL/QL parameter capture](https://github.com/bitfocus/companion-module-yamaha-rcp/blob/master/schemas/CLQL%20Parameters-1.txt)
declares input label color as a string. The same module's
[color choices](https://github.com/bitfocus/companion-module-yamaha-rcp/blob/master/rcpNames.json)
list Blue, Orange, Yellow, Purple, Cyan, Magenta, Red, Green and Off.
The fake accepts these quoted names and defaults to Blue. The performer screen
maps only these names to a swatch; Off and unavailable/unrecognized values have
a transparent swatch with an accessible label. Swatch RGB values are display
choices, not a claim about the console display's calibration. Color readback
never writes a console command. Physical-console verification remains CL5-8.

## Stereo roles and pan

The CL/QL parameter capture above separates `Mix/Role` (string, default Mono)
from `Mix/BusType` (string, default VARI), and records send pan as -63..63.
The bridge reads Role and only accepts phone pan with explicit StereoL or
StereoR readback; missing/unknown roles fail closed. It rechecks before flushing
queued pan writes. The UI hides pan for other roles and retains console pan
readback as its displayed value. Pan remains left/right in either orientation.
The fake exposes a test-only role setter for engineer simulation; the browser's
allow-list never permits role writes or linking buses. The fake's legacy
BusType default is not used to infer stereo. StereoR routing/pair behavior and
the role readback on physical hardware still require CL5-8 before the first show.
