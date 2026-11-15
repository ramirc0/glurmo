# rtviz

A live view of who holds which cores and GPUs on a Slurm cluster.

## Description

rtviz samples Slurm with `squeue`, `sinfo`, `scontrol`, `sdiag` and `sacct`. Each sample becomes a delta of starts, ends, core placements and queue changes. `rtviz serve` keeps the history in SQLite and streams the deltas over HTTP as server-sent events. `rtviz tui` follows that stream, or samples the cluster itself. The server also serves a web view of the same stream, sized for a desk or a wall display. Both views draw every core and GPU in the color of the user who holds it, beside panels for users, the queue, node health, and the last day of usage.

rtviz only reads. It never acts on Slurm and sends no notifications. It shows no per-person efficiency, idle, or failure counts.

## Getting Started

### Dependencies

* Linux on x86-64, with the Slurm client commands on `PATH`. rtviz is tested on Slurm 23.11.
* The history file must sit on a local disk, such as `/scratch`. SQLite locks are unreliable on NFS, Ceph or Lustre.
* Building needs Rust 1.100 with the `x86_64-unknown-linux-musl` and `wasm32-unknown-unknown` targets, [just](https://github.com/casey/just) and [trunk](https://trunkrs.dev). `rust-toolchain.toml` pins the version. `just static` builds the web view first and embeds it in the binary.

### Installing

Build the static binary and copy it onto your `PATH`. It runs on any x86-64 Linux host.

```
just static
cp target/x86_64-unknown-linux-musl/release/rtviz ~/.local/bin/
```

Settings live in `~/.config/rtviz/config.ron`. Every field is optional.

```ron
Config(
    sample: (jobs: "10s", nodes: "30s", partitions: "10m", stats: "60s", lookups: 25),
    store: (path: "/scratch/you/rtviz/zlurm.sqlite", keep: Forever, backfill: Days(30)),
    server: (listen: "127.0.0.1:8765"),
    tui: (server: "http://js029:8765", layout: "tui", theme: "rose-pine"),
    names: FullName,
    gpu_models: {"gpu": "V100", "gpuh200": "H200"},
)
```

`gpu_models` names the GPUs in each partition for clusters whose GRES lists no GPU type.

### Executing program

* Check what the cluster allows and what each command costs:
  ```
  rtviz probe
  ```
* Run the server. `systemd/rtviz.service` runs it as a user unit, and its header says how to install it.
  ```
  rtviz serve
  ```
* Follow a server in the terminal, or the server that the config file names:
  ```
  rtviz tui --server http://js029:8765
  rtviz tui
  ```
* Sample the cluster in-process when no server runs. The view then backfills the last day from `sacct`.
  ```
  rtviz tui --embedded
  ```
* Open the web view in a browser at the server's address, such as `http://js029:8765/`. The address can pick a layout, a theme and a viewer:
  ```
  http://js029:8765/?layout=tv&theme=rose-pine-dawn
  http://js029:8765/?me=otter
  ```
  `me` fills the `Mine` panel. The `tv` layout, or `tv` in the address, hides the cursor and lights up each user's cores in turn. Elsewhere, hovering a user, partition, or node row lights up its cores, and hovering the Die shows the job on a core or the state of a node. `t` and `l` cycle themes and layouts. `f` fills the screen with the panel under the pointer, and `f` or `esc` brings the layout back.
* The `Jobs` panel lists running and pending jobs. Jobs from one user that share a name, an array, or partitions collapse into one row, and a click opens it. Its heading searches user, full name, job ID, name, partition, node, and reason. Its chips narrow it to running, pending, your own, or GPU jobs. A click on a column heading sorts by it. Hovering a row lights its cores on the Die. Hovering a core on the Die lights its job's row, and hovering a node lights the rows of every job on it. A panel 1,200 pixels wide adds full names, time left and submit times. The terminal view shows the same rows, narrowed by `/`. The `jobs` layout sets the panel beside the Die.
* The web view's history panel is a timeline. The wheel or a drag pans it back, and ctrl with the wheel, or a pinch, zooms it. Type a span such as `6h` or `3d` in its heading, and *now* brings it back. A click replays the cluster from that moment, paused. The heading then offers 1×, 10×, 60× and 600×, pause, and *live*, and the keys `1` to `4`, `0` and `esc` do the same. The header shows the replayed time under a replay badge. The address can start a replay too, as in `?at=1794700000&speed=60`. A replay shows no load, free memory, pending reasons or job names, because the history does not keep them.
* Press `e` in the web view to edit its layout and theme on the live page. Click a panel to pick it, then split, remove, or change it in the sidebar. Drag a panel onto another to swap them, and drag the gap between panes to resize them. The theme side has a color picker for every color and imports a pasted base16 scheme. *Save in this browser* keeps the result in local storage. *Export RON* gives the file to save in the configuration directory below, which every page then picks up. `e` or `esc` closes the editor and drops unsaved changes.

The terminal view takes these keys:

| Key | Action |
| --- | --- |
| `q` | Quit |
| `tab` | Focus the next panel |
| `↑` `↓` `j` `k` | Scroll the focused panel |
| `/` | Filter by user, partition, or job name |
| `m` | Show only your jobs, or everyone again |
| `f` | Fill the screen with the focused panel, or show the layout again |
| `esc` | Clear the filter, then the fill |
| `l` | Next layout |
| `t` | Next theme |
| `?` | Help |

Themes and layouts are RON files. The built-in themes are `rose-pine`, `rose-pine-moon` and `rose-pine-dawn`, plus 20 base16 schemes such as `catppuccin-mocha`, `gruvbox-light` and `tokyo-night-dark`. `themes/` lists them all. The built-in layouts are `tui`, `web`, `tv` and `jobs`. A file in `~/.config/rtviz/themes/` or `~/.config/rtviz/layouts/` adds one, or replaces the built-in of the same name. The view reloads them within 2 seconds of an edit. See `themes/` and `layouts/` for the format.

## Help

* `rtviz --help` lists the commands, and `rtviz <command> --help` describes one.
* The terminal view writes its messages to `~/.local/state/rtviz/tui.log`, so they stay off the screen.
* In embedded mode the history panel stays empty until the first five-minute bin closes.
* A red banner in the header means the server or Slurm stopped answering. It says since when and why. The web view reconnects on its own and catches up without reloading.

## Authors

Christian Ramirez (christian.ramirez1@umassmed.edu)

## Acknowledgments

* The node and job listing rules follow [pestat](https://github.com/OleHolmNielsen/Slurm_tools/tree/master/pestat).
* The colors come from [Rosé Pine](https://rosepinetheme.com).
