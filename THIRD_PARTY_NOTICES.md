# Third-party notices

fz-maximize includes code ported from, or closely following, the following projects.

## Microsoft PowerToys (FancyZones)

<https://github.com/microsoft/PowerToys>

fz-maximize reimplements parts of FancyZones in Rust so that its zones match FancyZones
exactly. The ported logic covers layout geometry (`LayoutConfigurator`), monitor
identification and matching (`MonitorUtils`, `FancyZonesDataTypes`), layout-file parsing
(`AppliedLayouts`, `CustomLayouts`, `DefaultLayouts`), window placement
(`WindowUtils`) and DPI conversion (`dpi_aware`). It lives mainly in `src/fancyzones/`,
`src/monitors.rs`, `src/window.rs` and `src/filters.rs`. The exact upstream files and
versions are listed in `upstream/fancyzones-baseline.json`.

fz-maximize is not affiliated with or endorsed by Microsoft. "PowerToys" and
"FancyZones" are names of Microsoft products, used here only to describe compatibility.

```
The MIT License

Copyright (c) Microsoft Corporation. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

## Rust crates

The compiled program also includes these crates, all under MIT and/or Apache-2.0
licenses: `windows` (Microsoft), `serde`, `serde_json`, `toml`, `tracing`,
`tracing-subscriber` and `anyhow`, plus their dependencies. `cargo tree` lists the full
set; each crate's license is in its own package.
