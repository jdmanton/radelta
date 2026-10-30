//! Shape reports and display units for command output.

use super::layout::ResolvedLayout;
use std::io::Write;

pub(super) fn gb_per_s(raw_bytes: usize, seconds: f64) -> f64 {
    raw_bytes as f64 / 1_000_000_000.0 / seconds.max(1e-12)
}

pub(super) fn mib(n: usize) -> f64 {
    n as f64 / 1048576.0
}

pub(super) fn print_shape(layout: &ResolvedLayout) {
    let d = layout.dims;
    report!(
        "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
        d.t,
        d.c,
        d.z,
        d.y,
        d.x
    );
    report!("Layout source:   {}", layout.source);
    if layout.ifd_coords.is_some() {
        report!("Input mapping:   explicit OME TiffData IFD mapping");
    } else {
        report!(
            "Input page order: {} (slowest -> fastest)",
            layout.page_order
        );
    }
}
