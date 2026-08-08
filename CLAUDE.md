# Working on rmatrix

Digital rain for a terminal. Small, published (`rmatrix-reloaded` on crates.io),
and held to a higher bar than its size suggests — the point of the project is
that it is *nice*, and that includes the code.

Read this before changing anything. These are conventions the codebase already
follows, not aspirations.

## Layout

```
src/            the library — everything with behaviour lives here
  rain.rs       the simulation: cells, drops, glow, decay
  theme.rs      colour ramps, brightness quantisation, --levels
  charset.rs    glyph repertoires and the single-column width rules
  render.rs     the only module that produces bytes (damage tracking)
  writer.rs     the only module that hands bytes to a file descriptor
src/bin/rmatrix/  the binary — a thin wrapper, and kept thin
  main.rs       the event loop, and nothing else
  cli.rs        flags, Settings, validate()
  meter.rs      the stats readout behind `f`
  term.rs       raw mode, alternate screen, teardown, the frame Sink
  bench.rs      timing instrumentation behind --bench
tests/          integration tests that drive the real byte stream
examples/perf.rs  the performance harness the README quotes
```

The split exists so the simulation can be driven by tests without a tty. Keep it
that way: if you find yourself wanting a terminal in a unit test, the logic is in
the wrong place.

## The bar

Every change has to leave these true:

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

No `unwrap` or `expect` outside tests — `[lints.clippy]` in `Cargo.toml` denies
both, so this is a build failure, not a review note. `clippy.toml` exempts
`#[cfg(test)]` modules; integration tests under `tests/` are separate crates
that clippy does not count as test code, so they carry a crate-level allow. The
writer-thread spawn in `writer.rs` is the single sanctioned exception in
production code and carries its own `#[allow]` with a `reason`.

If you need a new exemption, write the `reason` first. If you cannot finish the
sentence, you want `?` instead.

No new dependencies without a real argument. No `unsafe`. The crate has neither
and that is a feature.

## Comments

Comments explain **why**, and cite the measurement when the decision was
empirical. Never restate the code.

The house voice, from `render.rs`:

```rust
// Row-major. Column-major looks tempting — a column is one drop's fade,
// so its colours are coherent and the pen could be reused — but it
// measures ~11% worse. At ~7% damage, lit cells are sparse in *both*
// axes, so neighbour coherence almost never applies, and scanning by
// column trades cheap same-row `MoveRight` hops for absolute moves
// (4.7 -> 8.2 bytes each). Measured, not assumed; see examples/perf.rs.
```

That comment is worth its length because it stops the next person redoing the
experiment. A comment saying `// scan the rows` would not be.

Where a constant was tuned, say what it was tuned against. Where a rejected
alternative is tempting, say why it lost.

## Tests

- **Names are full sentences.** `a_dark_cell_never_keeps_a_stale_glyph`, not
  `test_cell`. Read the list of test names and you should get a specification.
- **Assert observable behaviour, not implementation.** `tests/coalescing.rs`
  replays the emitted bytes through a model terminal rather than reaching into
  the renderer's private `prev` buffer — because `prev` is not the thing that
  can be wrong, the screen is.
- **A test that guards a specific bug says which bug, and why it mattered.**
  Whoever reads it next needs to know what breaks if they delete it.
- **Check your test has teeth.** Before committing a regression test, revert the
  fix and confirm it fails. `discarding_an_already_drawn_frame_corrupts_the_screen`
  is a negative control that exists purely to prove the other tests can detect
  corruption at all.
- Every behavioural claim gets a test. Every README performance claim gets a
  reproducible harness.

## Errors

User-facing errors go through `validate()` in `cli.rs` with `bail!`, and name the
offending flag and its value:

```rust
bail!("--density must be between 0.0 and 1.0, got {}", args.density);
```

`validate()` is pure and has no terminal, which is what makes every input rule
testable. Keep new validation there rather than in the library — library users
are free to feed the simulation anything, and the simulation is already safe
against it.

## Performance

The terminal emulator is the bottleneck, not this program. On a full-screen
vertical window rmatrix uses single-digit CPU while iTerm2 uses most of a core
parsing what it was sent. So:

- **Bytes emitted is the metric.** Not our own CPU time.
- Benchmark at a realistic size. The author runs 204x175 full-screen vertical;
  80x24 is not the interesting case.
- **Warm up properly.** The slowest drops fall 6 rows/sec, so a 175-row window
  needs ~29 *seconds* of simulated time to fill. Measuring a half-empty screen
  flatters every number by about 2x. This has bitten the harness before.
- Re-run `cargo run --release --example perf` after anything that touches
  `rain.rs`, `theme.rs` or `render.rs`, and update the README tables if the
  numbers move at the precision they print (five significant figures).
- Note that changing how often the simulation draws from the RNG changes the
  animation for a fixed seed — a single-seed comparison then measures a
  different realisation, not your change. Average over seeds when that happens.

Things already tried and rejected are documented in the README under "things
that didn't work". Read it before optimising; column-major scanning has been
measured and it is worse.

## Commits

One logical change per commit. The message explains **why**, in prose, with the
measurements that justified it. Look at `git log` — the bar is a paragraph or
three explaining the reasoning and what was verified, not a one-liner.

Do not commit or push unless asked.

## Releasing

Tags drive `.github/workflows/release.yml`. Published artifacts are
**immutable**: the workflow refuses to overwrite a release that already has
assets, because users may have verified against the old checksums. If a release
is wrong, bump the version.

Actions are pinned to commit SHAs with the version in a trailing comment. Keep
them that way when bumping.
