# Guide

## Install

Download `glurmo` from the [latest release](https://github.com/ramirc0/glurmo/releases/latest), or build it:

```
just static
cp target/x86_64-unknown-linux-musl/release/glurmo ~/.local/bin/
```

The binary is static and runs on any x86-64 Linux host. Building needs Rust 1.100 with the `x86_64-unknown-linux-musl` and `wasm32-unknown-unknown` targets, [just](https://github.com/casey/just) and [trunk](https://trunkrs.dev). `rust-toolchain.toml` pins the version. `just static` builds the web view first and embeds it in the binary.

`just install` builds the binary, installs it in `~/.local/bin`, copies the units from `systemd/` to `~/.config/systemd/user`, and installs logrotate's config as `~/.config/glurmo/logrotate.conf`. It then restarts glurmo if it is running. It leaves drop-ins alone, so the host settings below survive. It neither enables nor starts a unit.

The host must be able to sign requests as any Slurm client does: with munged and the cluster's key under `auth/munge`, or through slurmd's or sackd's socket under `auth/slurm`. glurmo finds the controller in slurm.conf, as Slurm's own tools do. A host without slurm.conf can name the controller in the config.

## Quick start

Check that glurmo can reach Slurm, and what each call costs the controller:

```sh
glurmo probe
```

Start the server:

```sh
glurmo serve
```

It samples the cluster, keeps history in a SQLite file, and serves the web view on `http://127.0.0.1:8765/`. A new store first reads 30 days of history from accounting, which can take a few minutes. From your own machine, forward the port and open that address:

```sh
ssh -L 8765:127.0.0.1:8765 login-node
```

`ctrl-c` stops the server. In another terminal, follow it in the terminal view:

```sh
glurmo tui --server http://127.0.0.1:8765
```

Set `tui: (server: "http://127.0.0.1:8765")` in the config to make that the default. With no server named, `glurmo tui` samples the cluster itself and backfills the last day. `--me otter` shows another login's jobs as yours, as `?me=` does on the web.

## Run as a service

A systemd user unit keeps the server running after you log out, and restarts it when it fails. The commands below are for bash. If your shell is fish or another, run `bash` first.

1. Install the binary as `~/.local/bin/glurmo`. Fetch the unit, the two that rotate its log, and logrotate's config:

   ```sh
   units=~/.config/systemd/user
   base=https://raw.githubusercontent.com/ramirc0/glurmo/main/systemd
   mkdir -p "$units" ~/.config/glurmo ~/.local/state/glurmo
   for unit in glurmo.service glurmo-logrotate.service glurmo-logrotate.timer; do
       curl -fsSL -o "$units/$unit" "$base/$unit"
   done
   curl -fsSL -o ~/.config/glurmo/logrotate.conf "$base/glurmo.logrotate"
   ```

2. Tie it to this host. Compute nodes often share your home directory, and a job that starts your user manager there would start a second server. The unit runs only where `ConditionHost` matches. Set it in a drop-in, a file in `glurmo.service.d/` whose lines override the unit's, so a new unit can replace the old one without losing them:

   ```sh
   for unit in glurmo.service glurmo-logrotate.service glurmo-logrotate.timer; do
       mkdir -p "$units/$unit.d"
       printf '[Unit]\nConditionHost=\nConditionHost=%s\n' "$(hostname)" > "$units/$unit.d/host.conf"
   done
   ```

   The empty `ConditionHost=` clears the unit's own value.

3. Check that your user manager outlives your login:

   ```sh
   loginctl show-user "$USER" -p Linger
   ```

   If it prints `Linger=no`, run `loginctl enable-linger`, or ask an admin to.

4. Start the server and the log rotation, and start them at boot:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now glurmo glurmo-logrotate.timer
   systemctl --user status glurmo
   ```

To upgrade, replace the binary and run `systemctl --user restart glurmo`, or run `just install` in a clone.

If `enable` fails with "Unit … is transient or generated", a unit of the same name started by `systemd-run` is in the way. Stop it, delete its file under `/run/user/$(id -u)/systemd/transient/`, and run `daemon-reload`.

### Logs

The units write their logs to `~/.local/state/glurmo`, because some clusters make the user journal unreadable.

| File | Holds |
| --- | --- |
| `serve.log` | glurmo's messages, and a line from systemd each time it stops |
| `postgres.log` | The Apptainer TimescaleDB's messages |
| `backup.log` | What `pg_dump` printed, and a line for each run |
| `logrotate.log` | Errors from the rotation |

glurmo's lines start with the local time. It logs a warning when a call, the store or a login lookup starts failing, and one line when it works again, with how long the failures lasted. Each hour it sums up the queue and how each kind of call fared, so a missing summary marks the server or host down.

`glurmo-logrotate.timer` rotates the logs weekly, or once one passes 50 MB, and keeps a year of them compressed. Read an old one with `zstdcat`. The newest rotated file stays uncompressed.

For more detail, set `GLURMO_LOG=debug` with `systemctl --user edit glurmo`:

```ini
[Service]
Environment=GLURMO_LOG=debug
```

`glurmo=debug` keeps other crates at their default level. The paths in the units are spelled out because systemd 252, as RHEL 9 ships it, expands `%S` to `~/.config` in a user unit.

## Configure

Settings live in `~/.config/glurmo/config.ron`. Every field is optional.

```ron
Config(
    slurm: (backend: Wire, controller: ["ctl-1:6817"]),
    sample: (jobs: "10s", nodes: "30s", partitions: "10m", lookups: 25),
    store: (keep: Forever, backfill: Days(30)),
    server: (listen: "127.0.0.1:8765"),
    tui: (server: "http://head:8765", layout: "tui", theme: "rose-pine", me: "otter"),
    names: FullName,
    groups: (hide: ["everyone"]),
    gpu_models: {"gpu": "V100", "gpuh200": "H200"},
)
```

* `slurm` picks the backend. `Wire` speaks Slurm's protocol. `Cli` runs `squeue`, `sinfo`, `scontrol`, `sdiag` and `sacct` instead, for a cluster newer than the protocol glurmo knows. `controller` overrides the addresses in slurm.conf.
* `sample` sets how often each kind of data is read. `lookups` caps placement lookups per tick, and only the `Cli` backend makes them.
* `store` keeps history in a SQLite file, `$XDG_STATE_HOME/glurmo/<cluster>.sqlite` by default, with a nightly copy in `$XDG_DATA_HOME/glurmo`. The file must sit on a local disk, because SQLite locks are unreliable on NFS, Ceph or Lustre. Where the home directory is on one of those, set `XDG_STATE_HOME` to a local disk, or name the file with `path`. `backfill` reads that much history from accounting into a new store. A TimescaleDB server can hold history in place of the file, as described below.
* `tui` picks the server the terminal view follows and its first layout and theme. `me` names whose jobs `m` and the `Mine` panel show, in place of `$USER`.
* `names` shows full names from the account database, or logins with `Login`.
* `groups` hides user groups by name, such as one that holds every user. glurmo reads each user's groups from the account database when it first sees the user, and again for everyone when it starts. A private group named after its login never shows.
* `gpu_models` names the GPUs in each partition, for clusters whose GRES lists no GPU type.

### History on TimescaleDB

SQLite needs nothing set up, but its file must sit on a local disk. A TimescaleDB server can hold history instead. Use one where no local disk keeps files across reboots, or where the site already runs PostgreSQL. It also compresses history older than 7 days.

You can run TimescaleDB yourself from its container image under Apptainer, with no admin rights, as below. To use a server that already exists, skip to [Use an existing PostgreSQL server](#use-an-existing-postgresql-server).

#### Set it up under Apptainer

These steps assume glurmo already runs as a service. Run them in bash, in one session, on the host that serves.

1. Name the paths the units expect. The units use these exact paths, so keep them:

   ```sh
   dir=~/.local/share/glurmo/timescaledb
   sif="$dir/timescaledb-2.32.0-pg18.sif"
   sock="$XDG_RUNTIME_DIR/glurmo-timescaledb"
   units=~/.config/systemd/user
   base=https://raw.githubusercontent.com/ramirc0/glurmo/main/systemd
   ```

2. Download the image, about 380 MB:

   ```sh
   mkdir -p "$dir"
   apptainer pull "$sif" docker://timescale/timescaledb:2.32.0-pg18
   ```

3. Create the database files. `initdb` makes an empty PostgreSQL data directory. Your login becomes its superuser, and peer authentication lets only the matching operating-system user in:

   ```sh
   apptainer exec --bind "$dir" "$sif" initdb -D "$dir/data" --auth-local=peer --auth-host=scram-sha-256
   ```

4. Let glurmo log in. glurmo runs as you but logs in as a role named `glurmo`, which peer authentication would refuse. A map in `pg_ident.conf` allows it. Its line in `pg_hba.conf` must come first, because the first matching line wins:

   ```sh
   echo "glurmo $USER glurmo" >> "$dir/data/pg_ident.conf"
   { echo 'local glurmo glurmo peer map=glurmo'; cat "$dir/data/pg_hba.conf"; } > "$dir/data/pg_hba.new"
   mv "$dir/data/pg_hba.new" "$dir/data/pg_hba.conf"
   ```

5. Install the server's units, with the same host drop-in as `glurmo.service`:

   ```sh
   for unit in glurmo-timescaledb.service glurmo-backup.service glurmo-backup.timer; do
       curl -fsSL -o "$units/$unit" "$base/$unit"
       mkdir -p "$units/$unit.d"
       printf '[Unit]\nConditionHost=\nConditionHost=%s\n' "$(hostname)" > "$units/$unit.d/host.conf"
   done
   ```

   Then make glurmo start after the server, in a second drop-in:

   ```sh
   printf '[Unit]\nAfter=glurmo-timescaledb.service\nWants=glurmo-timescaledb.service\n' > "$units/glurmo.service.d/timescaledb.conf"
   ```

6. Start the server. Its socket appears in `$sock`:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now glurmo-timescaledb
   ls -a "$sock"
   ```

   The listing shows `.s.PGSQL.5433`.

7. Create glurmo's role and its database. `psql` comes with the image:

   ```sh
   apptainer exec --bind "$sock" "$sif" psql -h "$sock" -p 5433 -d postgres \
       -c 'CREATE ROLE glurmo LOGIN' -c 'CREATE DATABASE glurmo OWNER glurmo'
   ```

   glurmo creates its tables and the `timescaledb` extension when it first starts.

8. Point glurmo at it. The config cannot expand variables, so print the socket's path:

   ```sh
   echo "$sock"
   ```

   Then set the store in `~/.config/glurmo/config.ron`, with that path as the host:

   ```ron
   store: (timescale: "host=/run/user/1000/glurmo-timescaledb port=5433 user=glurmo dbname=glurmo", backfill: Days(30)),
   ```

9. Optionally, keep your SQLite history. Accounting keeps no placements and no node health, so a new database has neither for the time before it. Copy the SQLite store in before glurmo first starts on the database:

   ```sh
   glurmo copy ~/.local/state/glurmo/CLUSTER.sqlite "host=$sock port=5433 user=glurmo dbname=glurmo"
   ```

   It prints each table's row count on both sides. glurmo may keep running on SQLite meanwhile. A copy that fails leaves the database empty, so it can run again.

10. Restart glurmo, and start the nightly backup:

    ```sh
    systemctl --user restart glurmo
    systemctl --user enable --now glurmo-backup.timer
    ```

    glurmo's first message now ends "with history in the TimescaleDB database glurmo".

The data stays in `$dir/data`. Each night `glurmo-backup.service` dumps it to `~/.local/share/glurmo/glurmo.dump`.

#### Use an existing PostgreSQL server

The server needs PostgreSQL 16 to 18 and TimescaleDB 2.29.2 or later. Earlier releases can miss open attempts in compressed chunks. glurmo is tested with TimescaleDB 2.32. The server must load the extension at start, in `postgresql.conf`:

```
shared_preload_libraries = 'timescaledb'
```

Give glurmo a role of its own that owns its database. It needs no superuser rights. As a superuser:

```sql
CREATE ROLE glurmo LOGIN;
CREATE DATABASE glurmo OWNER glurmo;
```

glurmo creates the `timescaledb` extension itself. The extension is trusted, so the database's owner may create it.

Name the server in `store`, with a PostgreSQL connection string in place of `path`. Setting both is an error.

* Over a Unix socket, name its directory as the host, as in `host=/run/postgresql user=glurmo dbname=glurmo`. Peer authentication lets in only the operating-system user of the role's name, so map your login to `glurmo` as step 4 above does.
* Over TCP, put `password=` in the connection string, and keep the config file readable by you alone. glurmo does not read `~/.pgpass`. The connection uses no TLS, so keep it on a network you trust.

Then follow step 9 to keep SQLite history, and restart glurmo. Back the database up with the server's tools, such as `pg_dump`.

#### The units

| Unit | Does |
| --- | --- |
| `glurmo-timescaledb.service` | Runs the server on `~/.local/share/glurmo/timescaledb/data`, with its socket in `$XDG_RUNTIME_DIR/glurmo-timescaledb`. It counts as started once `pg_isready` says the server takes connections, so glurmo starts after that. It stops with SIGINT, PostgreSQL's fast shutdown, so open connections do not hold it up. It logs lock waits over 1 s and statements over 5 s. |
| `glurmo-backup.service` | Dumps the database with `pg_dump` to `~/.local/share/glurmo/glurmo.dump`. It writes a `.part` file first, so a failed dump leaves the last good one. |
| `glurmo-backup.timer` | Runs the backup at 03:30, give or take 15 minutes. After downtime it runs at the next start. |

To restore a dump, stop glurmo, then recreate the database and load the dump between TimescaleDB's restore calls. The image must carry the same TimescaleDB version that made the dump. With the paths from step 1:

```sh
systemctl --user stop glurmo
apptainer exec --bind "$sock" "$sif" psql -h "$sock" -p 5433 -d postgres \
    -c 'DROP DATABASE glurmo' -c 'CREATE DATABASE glurmo OWNER glurmo'
apptainer exec --bind "$sock" "$sif" psql -h "$sock" -p 5433 -d glurmo \
    -c 'CREATE EXTENSION timescaledb' -c 'SELECT timescaledb_pre_restore()'
apptainer exec --bind "$sock" "$sif" pg_restore -h "$sock" -p 5433 -d glurmo ~/.local/share/glurmo/glurmo.dump
apptainer exec --bind "$sock" "$sif" psql -h "$sock" -p 5433 -d glurmo -c 'SELECT timescaledb_post_restore()'
systemctl --user start glurmo
```

#### Limits

A statement fails after 5 minutes, and a wait on a lock after 30 s. A failed write opens a gap, which accounting fills once writes work again. To change the limits, set them in the connection string's `options`, such as `options='-c lock_timeout=10s'`. A call that has waited 30 s checks the server with a query on a new connection. When that query gets no answer within 10 s, as from a stopped server or one stuck on its disk, the call fails. Later calls then fail at once until the server catches up. Over TCP, a lost connection fails a call within a minute.

#### Upkeep

* Attempts and usage bins are hypertables in 30-day chunks. Chunks older than 7 days are compressed where the server's license covers compression. The Apache-licensed build keeps them uncompressed, and glurmo says so once.
* `keep` prunes old history, as it does in SQLite.

## Web view

![The web view](media/web.png)

Open the server's address, such as `http://head:8765/`. The address can pick a layout, a theme and a viewer:

```
http://head:8765/?layout=tv&theme=rose-pine-dawn
http://head:8765/?me=otter
```

* `me` fills the `Mine` panel.
* The `tv` layout, or `tv` in the address, hides the cursor and lights up each user's cores in turn.

![The tv layout](media/web-tv.png)

* Hovering a user, partition or node row lights up its cores. Hovering a core shows its job, and hovering a node shows its state.
* The `Groups` panel lists user groups with what their members hold. A user in two groups counts in both. Hovering a group lights its members' cores. No built-in layout has it, so add it in the editor or a layout file.
* `t` and `l` cycle themes and layouts. `f` fills the screen with the panel under the pointer, and `f` or `esc` brings the layout back.
* The gear at the top right picks a theme or a layout by name and opens the editor. A wall display has no gear.

The `Jobs` panel lists running and pending jobs:

* Jobs from one user that share a name, an array or partitions collapse into one row, and a click opens it.
* Its heading searches user, full name, job ID, name, partition, node and reason. Its chips narrow it to running, pending, your own or GPU jobs.
* A click on a column heading sorts by it.
* Hovering a row lights its cores. Hovering a core lights its job's row, and hovering a node lights the rows of every job on it.
* At 1,200 pixels wide it adds full names, time left and submit times. The `jobs` layout sets it beside the cores.

![The jobs layout](media/web-jobs.png)

The history panel is a timeline:

* The wheel pans it back. Shift or ctrl with the wheel, or a pinch, zooms it around the pointer. A drag highlights a stretch, and letting go zooms to it. A white line marks the moment under the pointer, with its time.
* Type a span such as `6h` or `3d` in its heading, and *now* brings it back.
* A click replays the cluster from that moment, paused. The heading then offers 1×, 10×, 60× and 600×, pause and *live*. The keys `1` to `4`, `0` and `esc` do the same.
* The address can start a replay too, as in `?at=1794700000&speed=60`.
* A replay shows no load, free memory, pending reasons or job names, because the history does not keep them.

Press `e`, or pick *Edit layout and theme* from the gear, to edit the layout and theme on the live page:

* Click a panel to pick it, then split, remove or change it in the sidebar. Drag a panel onto another to swap them, and drag the gap between panes to resize them.
* The theme side has a color picker for every color and imports a pasted base16 scheme.
* *Save in this browser* keeps the result in local storage. *Export RON* gives the file to save in the configuration directory.
* `e` or `esc` closes the editor and drops unsaved changes.

## Terminal view

![The terminal view](media/tui.png)

| Key | Action |
| --- | --- |
| `q` | Quit |
| `tab` | Focus the next panel |
| `↑` `↓` `j` `k` | Scroll the focused panel |
| `/` | Filter by words that must all appear in a user, partition, node, job ID, job name or reason |
| `m` | Show only your jobs, or everyone again |
| `f` | Fill the screen with the focused panel, or show the layout again |
| `esc` | Clear the filter, then the fill |
| `l` | Next layout |
| `t` | Next theme |
| `?` | Help |

The terminal view writes its messages to `~/.local/state/glurmo/tui.log`. In embedded mode the history panel stays empty until the first five-minute bin closes.

## Themes and layouts

![The web view in Rosé Pine Dawn](media/web-dawn.png)

Themes and layouts are RON files. The built-in themes are `rose-pine`, `rose-pine-moon` and `rose-pine-dawn`, plus 20 base16 schemes such as `catppuccin-mocha`, `gruvbox-light` and `tokyo-night-dark`. The built-in layouts are `tui`, `web`, `tv` and `jobs`.

A file in `~/.config/glurmo/themes/` or `~/.config/glurmo/layouts/` adds one, or replaces the built-in of the same name. Views reload them within 2 seconds of an edit. See `themes/` and `layouts/` for the format.

## Recordings

* `glurmo record --minutes 30 OUT.jsonl` saves each call to Slurm and its answer. `--pseudonymize` rewrites logins, job names, free-text reasons and errors.
* `glurmo pseudonymize RAW OUT` rewrites a recording made without it.
* `glurmo serve --replay FILE` serves a recording with a throwaway history. `--speed 10` plays it ten times faster.
* `glurmo convert OLD NEW` turns a recording of command output, made before glurmo spoke the protocol, into one of calls and answers.

## Troubleshooting

* A red banner in the header means the server or Slurm stopped answering. It says since when and why. The web view reconnects on its own and catches up without reloading.
* The [logs](#logs) say when each kind of call started failing and when it worked again. Each hourly summary gives the slowest call of the hour.
* `backup.log` repeats a `pg_dump` warning about circular foreign keys on `continuous_agg`, a TimescaleDB table. It is harmless for the full dumps the backup makes.
* A controller older than 23.11 or newer than 27.05 rejects glurmo's protocols, so `glurmo probe` fails there. Set `slurm: (backend: Cli)`.
* Without munged or a sack socket, glurmo cannot sign requests and exits at once. With munged but the wrong key, every call fails with a munge error.
