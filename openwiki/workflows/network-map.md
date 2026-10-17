---
type: Workflow
title: Network Map - jump-host tree, hop editor, test connect, and `sshub map --dot`
description: The sixth dashboard tab (src/app/network_map.rs, src/tui/screens/map.rs) showing every real ProxyJump as a tree with blast-radius and tunnel lenses, a filter grammar, the hop editor in the host form, hop-by-hop test connect (src/ssh/test_connect.rs), and the Graphviz export `sshub map --dot` over the pure model in src/map/.
resource: src/app/network_map.rs
tags: [map, proxyjump, jump-hosts, hop-editor, test-connect, graphviz, workflow, tui]
---

# Network Map

Tab 6 (`active_tab == 5`, key `6`) draws the jump hosts as a left-to-right tree: `you -> hop -> hop`, with every host as a status glyph behind its last hop and a column counting what depends on each hop. Only real jumps are edges: a host's own ProxyJump, a group default it inherits, or an ssh_config `ProxyJump`. Groups and tags never create an edge. Design reference: `docs/network-map-design.md` and `docs/network-map/mockups/`.

## Code map

| Piece | File | Role |
|---|---|---|
| Model (pure, no UI) | `src/map/mod.rs`, `build.rs` | `collect_hosts` reads each host's effective ProxyJump (launcher hosts through `resolve_connection`, ssh_config hosts from the `ssh -G` values the sync stores, ProxyCommand presence from the sync fingerprint); `Map::build` resolves chains and builds the hop trie (dependents sigma, blast radius, clusters, jump loops). |
| Filter | `src/map/filter.rs` | `group:x` (group path contains), `tag:x` (exact tag), `via:x` (some hop name contains), `!x` (name does not contain), anything else fuzzy on the host name; terms split on spaces, double quotes group. |
| Hop helpers | `src/map/hops.rs` | ProxyJump string as a list (`none` = empty chain = direct), raw hop validation, routes in use, loop guard, `jump_problem`. |
| DOT export | `src/map/dot.rs`, `src/cli/map.rs` | `sshub map --dot`. |
| Tab state and keys | `src/app/network_map.rs` | `App::handle_key_map`, `MapView`, lenses, pickers, clicks. |
| Renderer | `src/tui/screens/map.rs` | Tree, path ribbon, facts, outline fallback. |
| Hop editor | `src/app/hop_editor.rs`, `src/tui/screens/hop_lines.rs` | The ProxyJump field of the host form. |
| Test connect | `src/ssh/test_connect.rs`, `src/app/test_connect.rs` | One `ssh ... true` per hop. |

## Chain rule

Matches `ssh -J` (verified against `ssh -v`) and `LauncherStore::expand_jump`: the first hop of a list brings its own ssh_config chain (ssh follows that host's `ProxyJump`), later hops are pinned by the list, and a launcher-only hop is expanded to `user@addr:port`, so its own jump setting is never followed. That is why one hop can sit under two parents. Hop tokens are untrusted text (imports write them): anything `[user@]host[:port]` refuses (a leading `-`, whitespace, shell characters) is an `Invalid` hop, never connectable and refused everywhere a jump reaches ssh. A ProxyCommand is one opaque `proxy cmd` hop whose text is never stored or shown. A jump loop is flagged and is not connectable.

Status glyphs come from the local ICMP ping: no reply is "unknown", never "down". Only a failed test connect marks a hop `×`.

## Map tab keys

All map actions are `KeyAction::Map*` (15 of them, rebindable); the rest reuse global actions. `App::handle_key_map` never falls through to the hosts-tab handler. While the `u` host picker, the `#` tag picker or the `/` filter line is open it owns the keyboard (digits typed into the filter do not switch tabs). Defaults:

| Key | Action |
|---|---|
| arrows (`MapUp/Down/Left/Right`, no `hjkl`) | Up/Down change row, Left/Right step back/forward along the path |
| `Enter` | Connect to the host, hop or raw hop under the cursor (normal connect path; a raw hop becomes an ad-hoc connect; on a cluster, "more" or direct-only row it folds; a loop host shows a notice, and the loop row opens a host of it for fixing) |
| `e` | Edit the host under the cursor (host form) |
| `/` | Filter line: typing filters live, `Tab` completes a term, `Enter` keeps it, `Esc` clears it |
| `#` | Tag picker; Enter toggles `tag:<name>` in the filter (needs 80x24) |
| `B` | Blast radius of the hop or host under the cursor (toggle); `n` / `N` step through the dependents; `b` broadcasts to them (`open_broadcast_for_hosts`); `y` copies their names |
| `o` | Tunnel lens: highlights tunnels riding on hops; `x` starts or stops the tunnel of the host under the cursor (several tunnels on one host: use the tunnels tab) |
| `u` | Use the hop under the cursor as the jump of another host: host picker (loop-forming hosts are listed last and refused), `Tab` toggles replace/append, saves the ProxyJump of a launcher host (ssh_config hosts are read-only here) |
| `t` | Test connect of the path under the cursor (see below); `Esc` cancels a running test or clears a finished one |
| `l` / `H` / `P` | After a failed test: `l` jumps to the hosts tab with the SSH log panel zoomed, `H` opens known hosts, `P` pushes a key to the failed hop |
| `y` | Copy the real `ssh` command for the path, shell-quoted (in the blast lens: the dependent host names) |
| `a` | On a raw hop: open the new-host form prefilled from it |
| `z` / `Alt+Enter` | Zoom the map body |
| `Space` / `Shift+Z` | Fold the row's cluster / fold all clusters (or expand direct-only) |
| `Shift+G` | Group manager |
| `Esc` | Clears, in order: a finished test, zoom, the lens, the filter |

Mouse: clicks select the cell drawn there (`ClickHit::MapCell`), double-click connects, the wheel moves the cursor row. Bracketed paste goes into the filter or the picker query (`map_paste`).

Big fleets cluster: more than `CLUSTER_ROOTS` (24) root hops are grouped by the hop's group (ungrouped ssh_config hops by domain suffix, then /24), an open cluster shows `CLUSTER_SHOWN` (12) root hops and folds the tail, `REVEAL_STEP` (20) more per reveal; when the map area is narrower than 80 columns or shorter than 16 rows the tree becomes an outline (one hop per row, depth as indent, selected path on the last line) and the tag picker and `u` are unavailable.

## Hop editor (host form)

The host form's ProxyJump field is a hop list (`src/app/hop_editor.rs`) over the unchanged comma separated string in `HostFormEdit::proxy_jump`. While the field is focused it takes (Tab, Esc and the save keys still reach the form):

| Key | Action |
|---|---|
| Up / Down | Move in the list (at the ends they leave the field) |
| `Enter` | Picker to replace the selected hop (or add the first) |
| `a` | Picker to add a hop after the selected one |
| `r` | Type a raw `[user@]host[:port]` (validated; commas refused) |
| `d` / `Delete` | Remove the hop; an empty list inherits the group's ProxyJump |
| `n` | `direct`: stores `none`, overriding a group jump |
| `X` | Clear the field (inherit the group's jump) |
| move-host-up/down chord (`KeyAction::MoveHostUp/Down`, `Ctrl+Up`/`Ctrl+Down` by default) | Reorder hops |
| `t` | Test connect of the chain as typed in the form, ending on the host being edited |
| `H` | Known-hosts manager, then back to the form |

The picker offers routes already in use first, then saved hosts (with `via <hop>` or `direct`), then `+ raw user@host:port`; hosts and routes that would loop back to the host being edited are marked forbidden and refused. A line under the list shows the dependents sigma, read from a map built once per open form. Push key and the ssh log need a saved host and the hosts tab, so they live on the map. The CLI refuses invalid or looping jumps the same way (`host add/edit`, `group add/edit` through `map::hops::jump_problem`).

## Test connect

`t` runs one step per hop (`ssh -o BatchMode=... -J h1,...,h(k-1) hk true`), stopping at the first failure (`src/ssh/test_connect.rs`, driven by `App::poll_test_run`). Properties:

- Runs through `SshCommandRunner` with `accept_new_host_keys = false`: nothing writes `known_hosts` (`StrictHostKeyChecking=yes`, `UpdateHostKeys=no`), an unknown key is reported as `host key unknown`, not trusted.
- Per step connect timeout 10 s, whole-step cap 30 s (`STEP_CONNECT_TIMEOUT_SECS`, `STEP_RUN_TIMEOUT`); Esc kills the whole process group (so an `ssh -W` hop helper dies too).
- A generated `-F` file (`isolated_config`, includes the user's config) makes ProxyCommand, prompts and known_hosts writes impossible, and ssh hands it to every `-J` helper. A step reached through a ProxyCommand is skipped, never executed; skipped steps are never reported as all ok.
- Stored secrets are fetched only when a step runs (`SecretSource`, `HopsSource`): the target's own secret never answers a hop, each earlier hop is answered with its own reference ([sessions](sessions-sftp.md)).
- `classify_failure` maps ssh stderr to short reasons: `auth failed`, `needs interactive auth` (the server offers only password or keyboard-interactive, which BatchMode cannot try), `key unreadable`, `timeout`, `refused`, `unknown host`, `host key unknown`, `host key changed` (detail: `known_hosts` file name and line), `proxy cmd failed`, else `failed (exit N)`.
- Oracle tests run real `ssh` against the shared `tempdir_sshd` in `src/test_support.rs`.

## `sshub map --dot`

`sshub map --dot` (`src/cli/map.rs`, help in `src/cli/help.rs`) prints a Graphviz `digraph sshub` of the same forest: `you` points at every root hop, a hop at the hops behind it, hosts hang off their last hop by dotted edges, each hop labelled with its dependents count. A hop shared by two chains is two nodes, as on screen. Jump loops are red, raw hops dashed, a ProxyCommand hop is one dashed `proxy cmd` box. `--dot` is required (no other formats); a closed pipe (`| head`) is not an error. It opens the profile like the other CLI commands (`CliContext`).

```bash
sshub map --dot | dot -Tsvg -o map.svg
```

## Change guidance

- Model changes belong in `src/map/` with tests in `src/map/tests.rs`; keep it free of UI and I/O.
- A new map key = `KeyAction::Map*` variant plus the `ALL`, label, field, default, `binds` and `set` arms in `src/keybinds.rs`, and a branch in `handle_key_map`.
- Anything that sends a jump to ssh must go through `map::hops::jump_problem` and `LauncherStore::expand_jump`, so connect, tunnel, broadcast, test connect and the map agree.
