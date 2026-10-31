# linux-audio-focus-daemon

`lafd` is a small Android-style audio focus daemon for MPRIS players on Linux.
It watches the session bus and pauses other players when a new player starts
playing. A trigger (call, alarm, notification sound) can hold playback paused
until it ends.

It replaces the earlier Go `audio-focus` service. The daemon uses the `dbus`
crate and runs as a single-threaded event loop plus one socket thread, with no
runtime and no async executor.

## Behaviour

- **New player starts playing** — it becomes the focus owner and every other
  playing MPRIS player is paused. The newest player wins.
- **Focus owner pauses or stops** — focus is released. Players paused by the
  service stay paused, matching Android's permanent focus loss.
- **Trigger player starts** — a player whose bus name or identity matches
  `trigger_players` gets a transient lock. Everything else is paused until
  that player pauses/stops or the trigger timeout expires. Players paused by
  the trigger are resumed afterwards.
- **Manual trigger** — any non-MPRIS event can call
  `lafdctl trigger start --timeout 60s call` and `lafdctl trigger end call`.
  New players that start during a trigger are paused too.
- **Startup** — one already-playing player is adopted without pausing
  anything. If a trigger player is already playing, the trigger lock is
  acquired and everything else is paused.

Only MPRIS-aware players are managed. Applications that play audio without
exporting `org.mpris.MediaPlayer2.*` are invisible to the daemon; use a manual
trigger for those.

## Install

One line:

```bash
git clone git@github.com:pharmaragist/linux-audio-focus-daemon.git ~/Projects/linux-audio-focus-daemon && ~/Projects/linux-audio-focus-daemon/install.sh
```

Or from an existing checkout:

```bash
cd ~/Projects/linux-audio-focus-daemon
./install.sh
```

The installer builds with Cargo, installs `lafd` and `lafdctl` to
`~/.local/bin`, installs `lafd.service` under `~/.config/systemd/user`,
installs a default config if one does not exist, disables the old
`audio-focus.service`, then enables and starts `lafd.service`.

Check it:

```bash
systemctl --user status lafd.service
lafdctl status
```

Logs:

```bash
journalctl --user -u lafd.service -f
```

Uninstall:

```bash
systemctl --user disable --now lafd.service
rm ~/.config/systemd/user/lafd.service
rm ~/.local/bin/lafd ~/.local/bin/lafdctl
systemctl --user daemon-reload
```

## Commands

```bash
lafdctl status
lafdctl trigger start --timeout 45s call
lafdctl trigger end call
lafdctl ping
lafdctl -j status
```

`--timeout` accepts `30s`, `500ms`, `2m`, or a plain number of seconds. A
timeout of `0` means no limit; a trigger then ends only on `trigger end`, when
its player stops, or when the daemon restarts.

`lafdctl -s PATH` overrides the socket path.

## Configuration

Config file:

```
~/.config/lafd/config.json
```

Default config:

```json
{
  "socket_path": "",
  "ignore": ["playerctld"],
  "trigger_players": ["ringtone", "alarm", "call", "phone"],
  "trigger_timeout_ms": 0,
  "resume_grace_ms": 1500
}
```

- `socket_path` — empty means `$XDG_RUNTIME_DIR/lafd.sock`.
- `ignore` — case-insensitive substrings matched against the bus name and
  identity. `playerctld` is always ignored.
- `trigger_players` — case-insensitive substrings that mark a player as a
  trigger.
- `trigger_timeout_ms` — default maximum trigger duration. `0` means no
  limit.
- `resume_grace_ms` — protection window for players resumed together, so they
  do not immediately steal focus from each other.

After editing the config:

```bash
systemctl --user restart lafd.service
```

## Socket protocol

The socket speaks newline-delimited JSON. One request per line, one response
per line:

```bash
printf '%s\n' '{"cmd":"status"}' \
  | socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/lafd.sock"
```

Requests:

```json
{"cmd":"status"}
{"cmd":"ping"}
{"cmd":"trigger_start","name":"call","timeout_ms":30000}
{"cmd":"trigger_end","name":"call"}
```

`timeout_ms` is optional. For `trigger_start`, `0` or a missing value uses the
configured default.

The socket is created with mode `0600` inside the per-user runtime directory,
so only the logged-in user can control the daemon.

## Memory

Measured on the development machine with three players tracked:

```text
ps RSS:        3.7 MB
anonymous RSS:  0.3 MB
PSS:            0.9 MB
threads:        2
```

Most of the RSS is shared libc/libdbus text; the private heap is a few hundred
kilobytes. For comparison, the Go version used about 4.9 MB RSS and 1.4 MB
anonymous memory with 7 threads.

## Design notes

- One thread owns the D-Bus connection. Socket commands are handed to it
  through a channel and answered in order, so focus state needs no locking.
- The main loop waits on D-Bus messages with a short timeout, which also
  bounds trigger-timeout and command latency.
- Pause and play calls run on the main thread with a 1 s timeout. A failed
  pause is removed from the resume set and logged.
- Track metadata is not read; only `Identity` and `PlaybackStatus` are used.
- `playerctld` is ignored because it proxies other players.
- A stale socket file left by a crash is detected and removed on startup.

## Development

```bash
~/.cargo/bin/cargo test
~/.cargo/bin/cargo build --release
```

## License

GPL-3.0-only. See `LICENSE`.
