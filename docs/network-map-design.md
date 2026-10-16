# Network map (Variant D, "Layers + Cargo"): implementation handoff

> Status: handoff from Claude Design (variant D), adopted for issue #79 on branch `feature/79-network-map`. Maintainer decisions that amend this brief are listed in the section "Maintainer amendments" at the end.

Issue #79 covers the jump/proxy UX and the interactive network map. This document is the build brief for the agent implementing it. The design is settled; your job is to build it as specified, in the slices listed at the end.

- **Design page (PNGs, captions, keys):** https://claude.ai/artifact/PzGYtvmQtxn7ocdFytTPom
- **Full design system (variants A to D, tokens, glyph vocabulary):** https://claude.ai/artifact/EVg2kLVL4Tz6BV8yv4j8Lb
- **Text mockups:** `docs/network-map/mockups/D*.txt`. Each file is the exact character grid of one screen (160×45; D09 is 100×30) and is the reference for layout and copy. The two links above are private to the maintainer; if you cannot open them, the `.txt` grids plus this document are complete on their own.

Colours are not in the `.txt` files. Every colour in this document is an existing SSHub theme role (`src/theme`, slots in `assets/themes/default.toml`). Never use a literal colour.

---

## 1. What you are building

1. **Tab 6 "map".** A left-to-right tree of jump hops. Each hop row ends in a **strip of status glyphs**, one per host whose *last* hop is that hop, grouped by host group. A **Σ** column on the right counts every host behind the hop at any depth. Below the map, a **path ribbon** of hop cards shows the selected path.
2. **Structured hop editor** in the host form, replacing the single ProxyJump text field.
3. **Test connect** for a chain, run from the map and from the hop editor.
4. **Lenses** on the map: tunnels (`o`), blast radius (`B`) and test progress (`t`).

The storage format does not change: ProxyJump stays the same comma-separated string in SQLite, ssh_config export and argv. All of this is UI plus a read-only resolution layer.

### Non-goals

- No drag-to-edit, no force or graph layout library, no animation beyond state changes.
- No parsing of ProxyCommand contents. It is shown as an opaque hop.
- No new async runtime. Test connect runs on a worker thread the same way ping does.

---

## 2. Locked decisions (do not reopen)

- **Navigation is arrows only.** No hjkl on the map: `h` is the global hosts-tab key and the maintainer rejected letter navigation.
- Hops shared by two chains are **drawn once per chain** and marked `◇`. The drawing is a forest, never a graph, so edges cannot cross.
- Groups and tags **never create edges**. They only filter, cluster and supply inherited values.
- Hosts are not tree nodes. They are glyphs in the strip of their last hop.
- The hop editor is a **vertical list** (not chips, not a route-only picker).
- Test connect never blocks Save.

---

## 3. Codebase map: where things live today

| Concern | Location | Notes |
|---|---|---|
| Tabs | `src/tui/widgets/tab_bar.rs` (`TABS`), `src/app/keys.rs` `try_tab_switch`, `App.active_tab: usize` (0-based) | Add `("6", "map")`, `active_tab = 5`, a `KeyAction::TabMap` bound to `"6"`. |
| Keybinds | `src/keybinds.rs`: `KeyAction`, defaults near line 404, labels near line 207, `binds()`/`set` arms | Every new action must be rebindable and appear in the keybind editor and `?` help. Mind the legacy-migration tests there. |
| Footer hints | `src/tui/mod.rs` `footer_keybinds` (~line 1178), `src/tui/widgets/footer.rs` | Add a `match` arm for the map tab and its sub-states (blast, filter, lens). |
| Group inheritance | `src/store/group_defaults.rs`: `ResolvedConnection::for_group_chain`, `LauncherStore::group_ancestor_chain`, `resolve_connection` | ProxyJump inherits nearest-group-first. Reuse this; do not reimplement it. |
| ssh_config hosts | `src/ssh/resolver.rs` (parses `proxyjump` at ~line 257), `src/ssh/import.rs`, `src/ssh/host.rs` (`-J` argv at ~line 51) | `has_proxy_command: bool` already exists on `SshHost` (presence only, value never stored or run; see the `parse_ssh_g_output_records_proxycommand_presence_only` test). |
| Host form | `src/app/host_form.rs`, `src/tui/screens/host_form.rs`, `HostFormField::ProxyJump` in `src/app/types.rs` (~line 1775) | The field is a `String` today; keep that as the stored value and put the list editor on top of it. |
| Group form | `src/tui/screens/group_form.rs`, `default_proxy_jump` in `src/app/types.rs` (~line 1351) | Optional slice 6: same hop editor for the group default. |
| Connect | `src/app/connect.rs` | Enter on the map must go through the normal connect path for the selected host. Do not build a new argv. |
| Ping / latency | `App.ping_data`, `App.ping_sample`, `ping_rx` (`src/app/mod.rs` ~line 407-448), `src/ping.rs` | Status glyph source: up / slow / down / unknown. |
| Tunnels | `src/tunnel/mod.rs` (`TunnelStatus`, `ReconnectPhase::Reconnecting`), `src/store/tunnels.rs`, `src/app/tunnels.rs` | Tunnel lens and `x` start/stop reuse the tunnels tab logic. |
| Sessions | `App.sessions` (`src/app/mod.rs` ~line 520) | `▶` marks a host with an open session. |
| Fuzzy search | `src/search.rs` (nucleo) | Map filter and hop picker. |
| Theme roles | `src/theme/catalog.rs`, `role_matrix.snapshot` | New component roles must be added to the catalog and snapshot (see `docs/theme-system.md`). |

### Verify first (before slice 2)

1. **Managed-only hops.** ssh resolves `-J bastion-eu` through `~/.ssh/config`. Check what happens today when a hop names a host that exists only in SQLite. If it does not connect, the map must not pretend it does: note it in the PR, and make test connect surface the failure honestly as `unknown host`. Fixing hop resolution for managed hosts is a separate issue unless the maintainer says otherwise.
2. **Oracle for chain resolution.** For ssh_config hosts, `ssh -F <cfg> -G <host>` prints the effective `proxyjump`. Per `docs/oracle-tests.md`, chain-resolution tests for ssh_config hosts must compare against it, not against our own parser. The existing `listed_aliases_round_trip_through_real_ssh` test in `src/ssh/resolver.rs` is the pattern to copy.

---

## 4. Data model: `src/map/` (new, pure, no UI)

```text
resolve_chain(host) -> Chain
  Chain = Vec<Hop> | Cycle(Vec<String>) | ProxyCommand
  Hop   = Saved { name, source } | Raw { user, host, port }
  source = Own | Inherited { group } | SshConfig
```

- Split the host's effective ProxyJump on `,`. Each token is a saved host if it matches a managed host name/alias or an ssh_config `Host`; otherwise it is a raw `user@host:port`.
- **Recursive:** a saved hop's own chain is prepended (`db-1` → `jump-2` → `jump-2` inherits `bastion-eu` → path `bastion-eu, jump-2, db-1`). Record the source of **each** hop, not only the last one.
- **Cycle detection:** a visited set during the recursion. On a repeat, return `Cycle` with the loop in order.
- **The host's own explicit chain overrides the hop's own chain.** `db-us-2 = "bastion-us,jump-2"` means jump-2 is reached via bastion-us here, even though jump-2's own setting says bastion-eu. This is why a hop can appear under two parents.

```text
HopTrie: node per distinct chain prefix
  node { key: path id e.g. "/bastion-us/jump-2", hop, children, cargo: Vec<(group, Vec<HostId>)>, sigma: usize }
```

- Build the trie by inserting every host's chain. The host lands in the `cargo` of the chain's final node. `sigma` = the size of the cargo across the whole subtree.
- `direct-only` = hosts with an empty chain that no other host jumps through. They live in a separate bucket, not in the trie.
- Hosts that are hops (bastion-eu) are both a root node and, if they have a chain, cargo elsewhere.
- **Blast radius of hop H** = every host whose resolved chain contains H at any depth. Count across all occurrences of H. db-us-2 is *not* in bastion-eu's radius.

Unit-test all of this without a terminal. The 26-host fictional fleet in the mockups (bastion-eu / jump-2 / bastion-us / corp-gw / raw hop / proxy cmd / loop-a⇄loop-b) is a good fixture.

---

## 5. Map rendering: `src/tui/screens/map.rs`

A custom `Widget` writing straight into the `Buffer`. Layout constants at 160 columns (see `D01Default.txt`):

| Column | x (inside panel) | Content |
|---|---|---|
| you | 2 | `◉ you` (accent, bold) |
| hop 1 | 12 | node: `glyph name ms` |
| hop 2 | 39 | node |
| cargo | 66 | `group ●●●…` strips, groups separated by 3 spaces, max 10 glyphs then `+n` |
| Σ | right-aligned, 4 wide | count |

- **Row assignment:** one DFS over the trie. Leaf → next row; parent → its first child's row… except that in D the parent's own row carries its cargo, so children start on the next row. Blank row between root subtrees (none in compact mode).
- **Edges:** root trunk at `col[1] - 3` (`┬ ├ └` + `─▸`); child trunk at `col[d+1] - 3`. On a parent row: `─` from node end to the trunk, `┬`, then the cargo leader `┈` to the cargo column. ProxyCommand edges use `┄`. Cycle edges use `══` / `╘══` in `error`.
- **Only two hop columns.** A deeper chain draws `‹ +n hops ›` (info, bold) between hop 1 and the final hop (D08).
- **Path highlight:** the cells along the selected path are re-coloured `accent`. The layout never changes.
- **Selection:** the cursor is either on a hop node (selection-bg across the node) or on one glyph (selection-bg on that cell plus `◂ name` after the strip in accent).

### Glyphs and roles

| Meaning | Glyph | Role |
|---|---|---|
| up / slow / down / unknown | `●` `●` `×` `○` | success / warning / error / unknown |
| session open | `▶` (replaces the status glyph in strips) | accent, bold |
| you | `◉` | accent |
| raw hop | `┆user@host:port┆` | text-muted |
| ProxyCommand | `‹proxy cmd›` | info |
| shared hop | `◇` after the name | info |
| cycle | `↻` + `══` | error |
| tunnel L/R/D | `⇄L` `⇄R` `⇄D`, colour = state | success running / warning reconnecting / text-muted stopped |
| test step | `·` `»` `✓` `✗` `–` | text-dim / connecting / success / error / text-dim |
| leader | `┈` | text-dim |
| dimmed (out of radius, filter context) | any | text-dim |

Every glyph is one cell. Use `unicode-width` to assert this in a test, and keep the set to the one above.

### Panels under the map

- **Path ribbon** (10 rows, full width): `◉ you ━━━▶ [card] ━━━▶ [card] ━━━▶ [target card]`. Cards are 30 wide × 5 tall: name + latency, `user@addr:port`, source line (`inherited · group "prod/db"`). Target card border = `border-focus`; a failed test card border = `error`. More than 4 cards wrap to a second line. Under 120 columns the ribbon becomes one line (`◉ you ━▶ ● a ━▶ ● b ━▶ ● c`).
- **Bottom row:** legend (left, 79) and facts (right, 78): address, jump source per hop, session/tunnel, the `ssh -J …` command.

### States (each has a mockup)

| Mockup | State |
|---|---|
| D01 | default, cursor on a root hop |
| D02 | glyph selected: path lit, ribbon with 3 cards |
| D03 | `B` blast: radius rows/edges/glyphs/Σ in `warning`, everything else `text-dim` (the cycle row too); title `blast radius · 12 hosts depend on bastion-eu` |
| D04 | `/ tag:db`: rows with no match hidden, hops above matches kept dim, non-matching glyphs in kept rows become `·` |
| D05 | `o` tunnel lens: hop badges, host glyphs → `⇄` in tunnel-state colour, others `·` |
| D06 | cycle selected: ribbon shows the loop in error; Enter opens the hop editor |
| D07 | empty: dim preview of the map, then 4 actions (`u`, `e`→`a`, `G`, `Shift+Z`) |
| D08 | 212 jump hosts, 1,840 hosts, depth 6: see §7 |
| D09 | 100×30 |
| D14 | `Shift+Z`: direct-only expanded, one glyph strip per group |

---

## 6. Keys

All are new `KeyAction`s scoped to the map tab unless marked as reuse. Defaults are free in `src/keybinds.rs` today. There are no keybind conflict tests in the repo (`src/keybinds.rs` tests cover migration, hints and defaults only); add a per-handler duplicate check or verify by hand. See section 14 for which "reuse" entries below are overridden.

| Key | Action |
|---|---|
| `↑` `↓` | Previous/next hop row. From a glyph, land on the nearest glyph (by column) of the next row. |
| `←` `→` | Walk along the row: you ← hop 1 ← hop 2 ← glyph 1 ← … ← last glyph. |
| `Enter` / double-click | Hop: connect to it. Glyph: connect to that host (normal connect path). Cycle: open the hop editor. Raw hop: ad-hoc connect. |
| `/` (reuse Search) | Map filter: `group:` `tag:` `via:` `!term`, plain = name. `Enter` keeps it, and in clustered mode reveals the match (§7). |
| `#` (reuse TagFilter) | Tag picker feeding the filter. |
| `B` | Blast lens toggle. `n` / `N` next/prev dependent; `b` (reuse Broadcast) opens broadcast preselected with the dependents. |
| `o` | Tunnel lens toggle. `x` (reuse ToggleTunnel semantics) starts/stops the selected tunnel. |
| `t` | Test the selected path. `Esc` cancels. |
| `u` | Use the selected hop as the jump for a host picked next (picker, replace/append toggle on `Tab` inside the picker, confirm). |
| `Space` (reuse ToggleGroup) | Fold/unfold a row's sub-rows, a cluster, or a long strip. |
| `Shift+Z` (reuse CollapseAll) | Fold all / expand the direct-only row. |
| `e` / `y` / `a` (reuse) | Edit host (form opens on the hop list) / copy `ssh -J …` / save a raw hop as a host. |
| `Esc` | Clear lens, then filter. |
| `z` / `Alt+Enter` (global) | Zoom the map panel. |

Mouse: click selects a hop or glyph, double-click connects, the wheel scrolls. You need a per-frame `(x, y) → target` hit map.

---

## 7. Scale rules (D08)

1. **Cluster roots** when there are more than 24 hop-1 rows. Key = the hop's own group. For ssh_config hops with no group, key = domain suffix, then /24. A cluster row shows `▾/▸ name  N hops` + a glyph strip of **the hops themselves** + union Σ.
2. **Sort by Σ** inside an open cluster. Show the top N that fit; fold the rest into `… 60 more hops · Σ ≤ 40 each`. `Space` reveals 20 more.
3. **Fold depth:** two hop columns maximum; middle hops become `‹ +n hops ›`.
4. **Down hop:** every glyph behind it renders `○`, its Σ in `error`, and the row adds `unreachable: hop is down`.
5. **Reveal:** filter + Enter opens only the clusters on the match's path and folds the rest; `Shift+Z` folds back.
6. Render only visible rows. Rebuild the trie on host/group change events, not every frame.

---

## 8. Hop editor (host form), see D10, D11, D13

- A list of hops with the title `hops ─ own value` or `hops ─ inherited`. Each row: index, status glyph, name, `saved · group`, `user@addr:port`, latency. Raw rows: `┆user@host:port┆ raw · not a saved host`.
- **Hint line inside the list (always visible):**
  - own value: `+ a saved host  r raw │ ↵ replace  d remove  Ctrl+↑↓ move  X clear → inherit`
  - inherited: `+ a saved host  r raw  (either replaces the inherited chain)`
- **Bottom border:** `saves as: bastion-eu,jump-2`, or `saves as: (empty) → inherits "jump-2"`.
- Under the list: `path  ◉ you ━▶ ● bastion-eu ━▶ ● jump-2 ━▶ db-9` (muted while inherited).
- **Picker** (`a`, or `Enter` on a row): fuzzy input; section 1 *reuse a route (replaces all hops)* lists routes already in use with health and host count; section 2 *one saved host* shows each host's own route (`via bastion-eu`); hosts that would create a cycle, including the host being edited, are greyed with `↻ would loop` and cannot be picked; the last row is `+ raw user@host:port` (`End` jumps there); a live `preview` line shows the resulting path.
- **Keys:** `↑↓` move (past either end → neighbouring form field, preserving the existing Tab/↓ form behaviour), `a`, `r`, `Enter`, `d`/`Delete`, `Ctrl+↑/↓` (same as move_host_up/down), `t` test, `X` clear own value. Do **not** use `Ctrl+R` (reveal_secret is active in the form) or `T` (import Termius).
- Raw input validates `[user@]host[:port]` as you type; an invalid token cannot be saved.
- ssh_config hosts: the list is read-only (no edit keys), source label `ssh_config`.

---

## 9. Test connect, see D12, D13

- Step *k* runs `ssh -o BatchMode=yes -o ConnectTimeout=10 -J h1,…,h(k-1) hk true`, using the same argv builder as connect so identity/user/port resolution match. Stop at the first failure.
- It runs on a worker thread with an mpsc channel back to the event loop, like ping. `Esc` kills the child.
- Map stderr/exit to short reasons: `auth failed` (Permission denied), `timeout`, `refused`, `host key changed` (REMOTE HOST IDENTIFICATION HAS CHANGED, include the `known_hosts:N` line when present), `unknown host`, `proxy cmd failed`.
- UI: per-hop state on the ribbon cards (map) or the card row in the form; the selected glyph shows `»` while running. The result line offers existing actions: `H` known hosts, `P` push key, `l` ssh log.
- **Oracle tests:** a tempdir `sshd` harness exists: `tempdir_sshd()` in the `session/mod.rs` test module (key auth on a free 127.0.0.1 port, skips when sshd is absent). Move it to a shared test util and reuse it (`docs/oracle-tests.md` still lists `session/`, `app/` as having no oracle). Exercise at least ok, auth-failed and timeout against a real `sshd`. If CI cannot run sshd, gate those tests behind an env var and say so in the PR. Never test the reason mapping against hand-written stderr alone.

---

## 10. Narrow terminals

- **100×30:** one-row header, hop columns at x 10/31, cargo at 52, no latency, 6 glyphs then `+n`, ribbon as one line (D09).
- **Below 80×24:** outline mode. One node per row, depth as indent (`  ├ jump-2`), and the selected path as `you › bastion-eu › jump-2 › db-1` on the last line (truncate from the left). Keys still work.
- **Below 40×12:** a single line, `map needs 40×12, press 1 for hosts`.

---

## 11. Delivery slices (one PR each, into `development`)

1. **`src/map` model:** chain resolution, cycle detection, trie, Σ, blast radius, ProxyCommand presence flag in the resolver. Unit tests plus `ssh -G` oracle tests. No UI.
2. **Map tab, read-only:** tab 6, rendering of D01/D02/D06/D07, arrow navigation, ribbon, facts, Enter to connect, footer hints, `?` help entries.
3. **Lenses and filter:** `B` (with n/N, b), `/` + `#`, `o` + `x` (D03-D05).
4. **Scale:** clustering, Σ sort with tail fold, depth fold, down-hop rows, reveal, direct-only expansion (D08, D14), narrow modes (D09, outline).
5. **Hop editor + `u`:** list editor, picker with route reuse, cycle guard, raw validation (D10, D11).
6. **Test connect** (map + editor) with oracle tests (D12, D13). Optionally, the hop editor for group defaults.

Each slice: `cargo fmt`, `cargo fmt --check`, `cargo clippy --all-targets`, `just test`, green CI before done (see CLAUDE.md). Add e2e scenarios in `tests/e2e` using `TestBackend`, and compare against the `.txt` grids where practical: assert key rows, not full-screen equality, since data differs. Add a CHANGELOG `[Unreleased]` entry per slice. Branches are `feature/network-map-*` cut from `development` (`scripts/check-branch-name.sh`). Sign every GitHub comment as `docs/implementation-flow.md` requires.

## 12. Acceptance checklist

- [ ] ProxyJump storage, export and argv byte-identical to before for every existing host.
- [ ] Map shows only real edges (own / group-inherited / ssh_config). Groups and tags never add one.
- [ ] Shared hop drawn per chain with `◇`; blast radius counts across all occurrences; db-us-2 case correct.
- [ ] Cycle: shown, not connectable, Enter opens the editor; the picker refuses to create one.
- [ ] Raw hop and ProxyCommand drawn as specified; ProxyCommand is never parsed or executed by the map.
- [ ] Arrow keys only; every new action rebindable, in the footer and `?` help, no duplicate bindings within the map handler (there is no keybind conflict test today, see section 14).
- [ ] All colours via theme roles; role snapshot updated; looks right in every built-in theme.
- [ ] 212-jump-host fixture renders within one frame budget (50 ms loop) and stays navigable.
- [ ] 100×30, outline (<80×24) and too-small (<40×12) modes work.
- [ ] Test connect: per-hop states, stops at first failure, cancellable, reasons mapped, never blocks Save, oracle-tested.

## 13. Maintainer amendments

- Scope is the whole of #79: this design, the structured hop editor, per-hop test connect, the map tab, map actions and the tunnel overlay, plus `sshub map --dot` (Graphviz export).
- Edges are real jumps only: own ProxyJump, group-inherited, ssh_config (via `ssh -G`). Unknown raw hops are dimmed placeholder nodes, ProxyCommand is an opaque "proxy cmd" badge, cycles are errors.
- Test connect uses BatchMode and the stored-secret askpass path like the existing connect paths.
- No em or en dashes in new copy. Mockup text that contains them (for example "none — reached directly") is reference for layout only; use a comma, colon or hyphen in the shipped strings.

## 14. Review amendments

Decisions from the adversarial review against the code. They override sections 4 to 10 where they differ.

1. **Hop resolution.** ssh only sees the `-J` string. A SQLite-only hop is expanded to `user@addr:port` in the shared `-J` builder (the one place argv is built), so it connects without a `~/.ssh/config` alias. Per-hop identity, certificate and group defaults are not carried to a hop. Chain rule: the first hop's own ssh_config chain is prepended by ssh itself (map it via `ssh -G <hop>`); every later hop is pinned by the command-line `-J`, so its own chain is ignored. SQLite or group chains of a hop are never recursed into. Hop cards show the address actually used. Tokens may be `ssh://user@host:port` or `[::1]:22`; strip `user@` and `:port` before matching a saved host.
2. **Chain source.** `SshConfig` and `Legacy` hosts take the chain from cached `ssh -G` output, resolved on a worker thread (cache keyed by `ssh_config_hash`; the map shows the stale data meanwhile). `Launcher` hosts take the stored or group-inherited value via `resolve_connection`. The token `none` means an empty chain (not a hop named "none"), and the hop editor offers a "direct (override group)" option that stores `none`. Known limit: wildcard `Host` blocks in `~/.ssh/config` can still add a ProxyJump or ProxyCommand to a Launcher host that SQLite does not show.
3. **Hop token validation.** Reject any token whose user or host starts with `-`. Parse raw hops with the existing `parse_adhoc_target` (`src/app/adhoc.rs`), not a new validator. Every hop target goes through `safe_ssh_target`. Only aliases listed by `list_hosts` are passed to `ssh -G`. Test with `-oProxyCommand=...` as a hop name.
4. **ProxyCommand.** `Hop::ProxyCommand` carries no text; the map shows the fixed label `‹proxy cmd›` and never the command (mockups corrected). `ssh -G` reports only one of `ProxyJump` and `ProxyCommand` for a host (the first one in the config wins, checked against OpenSSH 10.5), so a host with a ProxyCommand is drawn as that one opaque hop and keeps no other hops. Test connect skips any step whose host has `has_proxy_command` and shows it as `–`, and every step that would go through such a hop (`-J` prefix) is skipped too ("proxy cmd, not tested"); the map never parses or spawns a ProxyCommand itself.
5. **Health.** `ping.rs` is a local ICMP probe (`ping -c 1 -W 1`). The map never infers "hop is down" or "unreachable: hop is down" from it. Glyphs say "ICMP from here"; a hop is shown down only after a failed test connect of that hop. Hosts without a ping reading are `unknown`.
6. **Keys and handlers.** Tab 6 has its own handler (`5 => handle_key_map` in `handle_key_dashboard_tab`) with no fall-through to `handle_key_normal`, a `TabMap` branch in `try_tab_switch`, and `TABS` grows to six. The map has its own filter state and mode (no reuse of `AppMode::Search` or `TagFilter`, whose Enter connects the hosts selection) and its own grammar (`group:`, `tag:`, `via:`, `!term`) on top of the nucleo matcher. Navigation uses new arrow-only `MapUp/Down/Left/Right` actions, not `move_up/down` (which carry `j/k/l`). New map actions replace the "reuse" entries for `y` (copy `ssh -J`, not `yank_log`), `Space`, `Shift+Z` (map fold, not group fold) and `x` (a `MapToggleTunnel` start/stop; `tunnel_kill` only stops). Tunnel start/stop is refactored to take a tunnel id instead of `tunnel_selected`. `z` and `Alt+Enter` zoom are handled inside the map handler. `b` needs a new `BroadcastTarget::Hosts(Vec<id>)` and a setup entry that skips the menu.
7. **Test connect.** Reuse `SshCommandRunner` (BatchMode chosen by stored secret, timeout, cancel). Add the reason `host key unknown` (first connection, "Host key verification failed"). Do not pass `StrictHostKeyChecking=accept-new`: a diagnostic must not write `known_hosts`. Kill the whole process group on Esc (`spawn_group` and `kill_group` from `src/cli/exec.rs`), so the `ssh -W` helper dies too. Askpass never answers a jump hop's prompt (#141), so a password-only hop reports `auth failed`. Oracle tests share `tempdir_sshd`.
8. **Cycles and blast radius.** Cycle detection uses a visited set per resolution path, seeded with the target host, so a hop shared by two parents is not a cycle. Hosts in a cycle have no chain but count as dependents of every hop in the loop, in blast radius and Σ.

## 15. Implementation notes after review

- **Test connect never writes known_hosts.** The runner adds `-o StrictHostKeyChecking=yes -o UpdateHostKeys=no` (command-line options beat a user config that says `accept-new`); an oracle test with such a config proves `known_hosts` untouched.
- **Hop tokens are kept as written.** `user@host:port` overrides reach the `-J` prefix of a test exactly as a real connect sends them (`Hop::token`). A launcher hop whose stored address or user is not one valid hop (comma, second `@`, whitespace) is left as the token was written.
- **A bad stored ProxyJump hop** is a host notice on connect and push key, never an error that ends the TUI.
- **Map keys beyond the first slices:** `y` copies the `ssh -J` command, `a` on a raw hop opens the new-host form filled from it, `z` / `Alt+Enter` zoom the map, `H` / `P` / `l` follow a failed test, click selects, double-click connects, the wheel moves the cursor. The tag and use-as-jump pickers are not opened in outline mode (they would have no room to draw).
- **Rebuild cost.** Collecting 1,840 hosts behind 212 jumps takes about 0.4 s in a debug build (one pass of SQLite reads per host); it runs on open and on a host reload, never per frame. The hop editor builds its map once per open form.
