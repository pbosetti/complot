# complot

Real-time plots driven from the shell, loosely inspired by the grammar of
graphics. Every plot is its own window (a small GUI process); short-lived
`complot` calls push data to it by name.

```sh
h=$(complot --aes "x=t y=temp g=sensor")   # open a window, keep its handle
complot $h log.csv                          # push a CSV chunk
complot $h -x 3.2 -y 21.5 -g kitchen        # push a single point
tail -f log.csv | complot $h -              # stream stdin, live
complot $h --close
```

## Usage

```
complot [NAME] [FILES...] [--aes SPEC] [-x X] [-y Y] [-g GROUP]
        [--create] [--clear] [--close] [--title TEXT]
complot --list
complot --close-all
```

| Invocation | Effect |
|---|---|
| `complot` | Opens a new plot with an automatic name (`plot1`, …) and prints it. |
| `complot NAME` | Opens plot `NAME` (or reconfigures it if already open) and prints `NAME`. |
| `complot NAME -y V [-x V] [-g G]` | Pushes one point. Without `-x`, x is the series' sample index. |
| `complot NAME FILE...` | Pushes CSV files; `-` streams stdin line by line. |
| `--aes "x=c1 y=c2 g=c3"` | Sets the aesthetic mapping; it is kept by the plot for later chunks. |
| `--create` / `-c` | Opens the plot if needed when pushing data. |
| `--clear`, `--close`, `--title T` | Clears the data, closes the window, sets the title. |
| `--list` / `-l` | Lists the open plots. |
| `--close-all` | Closes every open plot. |

Pushing data to a plot that is not open is an error (exit status 1), so a
loop stops when you close its window:

```sh
h=$(complot)
while sleep 0.2; do complot $h -y $RANDOM || break; done
```

## Aesthetics

`--aes` takes whitespace-separated `key=column` pairs:

- `x`: x position. If unmapped, x is the row index of each series.
- `y`: y position; a comma-separated list (`y=a,b,c`) plots one series per
  column.
- `g` (also `group`, `color`): grouping column; each distinct value is a series
  with its own colour.

Columns are referenced by header name, or by 1-based index when no header name
matches (`x=1 y=3`). Without an aesthetic, column 1 is x and every other column
is a y series.

## CSV chunks

Each file (or stdin stream) is a chunk. The separator is detected on the first
line (`,`, tab, `;`, then whitespace) and `#` lines are comments. The first
line is a header if none of its fields is a number; a chunk without a header
reuses the last header the plot has seen. Missing values (`""`, `NA`, `NaN`,
`null`) are skipped. Quoted fields with embedded separators are not supported.

## The window

Drag or scroll to pan, Ctrl/⌘+scroll or pinch to zoom, right-drag to
box-zoom, double-click to fit. The side panel holds the few graphical settings:
geometry (points, lines, both), point size, line width, legend, grid, follow
(auto-fit while data arrives), a per-series buffer limit for rolling windows,
clearing the data and copying it as CSV.

Large series (over 5,000 points) are decimated for drawing only; the stored
data stays complete. Lines keep the first, last, minimum and maximum point of
every pixel column, so peaks are never lost; markers keep one point per
marker-sized screen cell. A million points draw in about 130 MB. A large
series whose x values are not increasing is drawn as points even in line mode,
since its line cannot be decimated. The status line says when decimation is
active (`drawing N of M`).

## Build

```sh
cargo build --release   # binary: target/release/complot
```

Plots are registered in a per-user directory (`$XDG_RUNTIME_DIR` or the
system temp dir, under `complot-$USER`; override with `COMPLOT_DIR`), which
also holds each plot's log. Clients talk to plots over TCP on 127.0.0.1,
authenticated with a per-plot random token.
