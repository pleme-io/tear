# tear attach

A terminal client for the tear daemon. Pick a session, work in it, detach, come back later. The session and its shell keep running while you are away.

## Run it

| Where | Command |
|---|---|
| Local daemon | `tear attach` (picker) or `tear attach work` |
| Over ssh | `ssh -t rio tear attach` (`-t` is required: the client needs a tty) |
| Remote daemon over TCP | `tear attach --tcp host:port` (set `TEAR_AUTH_TOKEN` if the daemon needs one) |
| Other socket | `tear attach --socket /path/tear.sock` |

A target is a session name, a session id, or a unique prefix of either. A second match is an error that lists the candidates.

## Picker

Built on `tear-picker` (see `docs/PICKER.md`), the same model as mado's Ctrl-S.

| Key | Action |
|---|---|
| type | fuzzy filter |
| Up / Down, ctrl+p / ctrl+n | move |
| Enter | attach; on a `create` row, create and attach |
| n (empty filter) | new session; type a name, Enter |
| x or ctrl+d | kill, after a `y` confirmation |
| ctrl+e | rename |
| Esc, q (empty filter) | quit; inside a session, return to it |

`n`, `x` and `q` are commands only while the filter is empty. Set `attach.picker_shortcuts: false` to turn them into filter characters.

Each row shows a live or latent badge, the pane count and the age of the session (time since creation; the daemon keeps no last-activity stamp). A latent session has no running pane. It cannot be attached, only killed or renamed.

## Attached mode

Raw tty. Your keys go to the pane. The pane streams to your terminal after a replay of its history and modes.

| Key (default) | Action |
|---|---|
| ctrl+s | open the picker over the session; Esc returns, Enter hops |
| ctrl+b then d | detach |
| ctrl+b ctrl+b | send one literal ctrl+b |
| ctrl+b then any other key | both keys go to the pane; nothing is swallowed |

A closed input (dropped ssh) detaches. It never ends the pane.

## Sizing and multi-attach

The client sizes the pane to its terminal before the history is replayed, and again on every SIGWINCH. Replaying an 80x24 grid into a smaller terminal clips it. `attach.sizing: after_replay` reverses the order.

Several clients can attach to one pane. Each sees the same output. The last client to attach or resize sets the pane size; the others see it at that size. A client that detaches never ends another client's attach.

## Config (`attach:` in tear.yaml)

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Off makes `tear attach` refuse with a message. |
| `picker_enabled` | `true` | Off passes the picker key to the pane. |
| `picker_key` | `ctrl+s` | Open the picker while attached. |
| `detach_prefix` | none (uses top-level `prefix`, `ctrl+b`) | First key of the detach chord. |
| `detach_key` | `d` | Second key of the detach chord. |
| `refuse` | `ctrl+c ctrl+d ctrl+z ctrl+l ctrl+r ctrl+[ ctrl+m ctrl+j ctrl+i ctrl+h` | Keys the pane needs. Binding the picker key or the prefix to one is refused at start with a message. Remove a key from the list to allow it. |
| `sizing` | `before_replay` | `before_replay` or `after_replay`. |
| `picker_shortcuts` | `true` | The `n` `x` `q` commands. |

Key forms: `ctrl+<letter>`, `C-<letter>`, `^<letter>`, `ctrl+space`, a single character, `esc`, `enter`, `tab`, `space`, `backspace`.

## States

| State | What you see |
|---|---|
| Daemon unreachable | `tear-daemon is not reachable at ...`, exit 1 |
| No tty | `tear attach needs a terminal on stdin and stdout`, exit 1 |
| No sessions | picker with `no sessions yet: n or Enter creates one` |
| Daemon lost in the picker | the last list stays, marked `daemon unreachable` |
| Daemon lost while attached | `lost the connection to tear-daemon`, exit 1, pane untouched |
| Pane is input-locked | a one-line `input refused` notice on the bottom row |
| Refused binding | `... is on the refuse list ...`, exit 1 |

## What the host must run

`tear daemon` must be running on the host (a systemd or launchd user unit) and the `tear` CLI must be on the PATH of the ssh login shell.
