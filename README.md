# rtviz

A live view of who holds which cores and GPUs on a Slurm cluster.

## Description

rtviz samples Slurm with `squeue`, `sinfo`, `scontrol`, `sdiag` and `sacct`. Each sample becomes a delta of starts, ends, core placements and queue changes. `rtviz serve` keeps the history in SQLite and streams the deltas over HTTP as server-sent events. `rtviz tui` follows that stream, or samples the cluster itself. It draws every core and GPU in the color of the user who holds it, beside panels for users, the queue, node health, and the last day of usage.

rtviz only reads. It never acts on Slurm and sends no notifications. It shows no per-person efficiency, idle, or failure counts.

## Getting Started

### Dependencies

* Linux on x86-64, with the Slurm client commands on `PATH`. rtviz is tested on Slurm 23.11.
* The history file must sit on a local disk, such as `/scratch`. SQLite locks are unreliable on NFS, Ceph or Lustre.
* Building needs Rust 1.100 with the `x86_64-unknown-linux-musl` target, and [just](https://github.com/casey/just). `rust-toolchain.toml` pins the version.

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

The terminal view takes these keys:

| Key | Action |
| --- | --- |
| `q` | Quit |
| `tab` | Focus the next panel |
| `↑` `↓` `j` `k` | Scroll the focused panel |
| `/` | Filter by user, partition, or job name |
| `m` | Show only your jobs, or everyone again |
| `esc` | Clear the filter |
| `l` | Next layout |
| `t` | Next theme |
| `?` | Help |

Themes and layouts are RON files. The built-in themes are `rose-pine`, `rose-pine-moon` and `rose-pine-dawn`. The built-in layouts are `tui`, `web` and `tv`. A file in `~/.config/rtviz/themes/` or `~/.config/rtviz/layouts/` adds one, or replaces the built-in of the same name. The view reloads them within 2 seconds of an edit. See `themes/` and `layouts/` for the format.

## Help

* `rtviz --help` lists the commands, and `rtviz <command> --help` describes one.
* The terminal view writes its messages to `~/.local/state/rtviz/tui.log`, so they stay off the screen.
* In embedded mode the history panel stays empty until the first five-minute bin closes.
* A red banner in the header means the server or Slurm stopped answering. It says since when and why.

## Authors

Christian Ramirez (christian.ramirez1@umassmed.edu)

## Acknowledgments

* The node and job listing rules follow [pestat](https://github.com/OleHolmNielsen/Slurm_tools/tree/master/pestat).
* The colors come from [Rosé Pine](https://rosepinetheme.com).
