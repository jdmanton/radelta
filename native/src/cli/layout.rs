//! ImageJ/OME layout inference and canonical TCZYX plane mapping.

use radelta_native::{Dims5, DEFAULT_STREAM_MEMORY_MIB};
use std::path::Path;

mod ome;
use ome::infer_ome_layout;

#[derive(Debug, Clone)]
pub(super) struct DatasetArgs {
    pub(super) t: Option<usize>,
    pub(super) c: Option<usize>,
    pub(super) z: Option<usize>,
    pub(super) page_order: Option<String>,
    pub(super) ignore_metadata: bool,
    pub(super) memory_mib: usize,
    pub(super) force_stream: bool,
}

impl Default for DatasetArgs {
    fn default() -> Self {
        Self {
            t: None,
            c: None,
            z: None,
            page_order: None,
            ignore_metadata: false,
            memory_mib: DEFAULT_STREAM_MEMORY_MIB,
            force_stream: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InferredLayout {
    pub(crate) t: usize,
    pub(crate) c: usize,
    pub(crate) z: usize,
    pub(crate) page_order: String,
    pub(crate) source: String,
    /// Optional exact TIFF-IFD -> (T,C,Z) mapping, used for OME-TIFF
    /// files whose TiffData elements explicitly override DimensionOrder.
    pub(crate) ifd_coords: Option<Vec<(usize, usize, usize)>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedLayout {
    pub(crate) dims: Dims5,
    pub(crate) page_order: String,
    pub(crate) source: String,
    pub(crate) ifd_coords: Option<Vec<(usize, usize, usize)>>,
}

pub(crate) fn validate_page_order(order: &str) -> Result<(), String> {
    let b = order.as_bytes();
    if b.len() != 3 {
        return Err("--page-order must be a permutation of TCZ".into());
    }
    let mut v = b.to_vec();
    v.sort_unstable();
    if v.as_slice() != b"CTZ" {
        return Err("--page-order must be a permutation of TCZ".into());
    }
    Ok(())
}

pub(super) fn imagej_value(desc: &str, key: &str) -> Option<usize> {
    desc.lines().find_map(|line| {
        let line = line.trim();
        let (k, v) = line.split_once('=')?;
        if k.trim().eq_ignore_ascii_case(key) {
            v.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

fn infer_imagej_layout(desc: &str, pages: usize) -> Result<Option<InferredLayout>, String> {
    if !desc
        .lines()
        .any(|line| line.trim_start().starts_with("ImageJ="))
    {
        return Ok(None);
    }
    let c = imagej_value(desc, "channels").unwrap_or(1);
    let t = imagej_value(desc, "frames").unwrap_or(1);
    if c == 0 || t == 0 {
        return Err("ImageJ channels/frames must be positive".into());
    }
    let z = match imagej_value(desc, "slices") {
        Some(v) => v,
        None => {
            let total = imagej_value(desc, "images").unwrap_or(pages);
            let ct = c.checked_mul(t).ok_or("ImageJ C*T overflow")?;
            if ct == 0 || !total.is_multiple_of(ct) {
                return Err("cannot infer ImageJ slices from images/(channels*frames)".into());
            }
            total / ct
        }
    };
    if z == 0 {
        return Err("ImageJ slices must be positive".into());
    }
    let product = c
        .checked_mul(z)
        .and_then(|v| v.checked_mul(t))
        .ok_or("ImageJ T*C*Z overflow")?;
    if product != pages {
        return Err(format!(
            "ImageJ metadata says C*Z*T={product}, but TIFF contains {pages} pages"
        ));
    }
    // ImageJ hyperstacks are stored C fastest, then Z, then T (CZT).
    // Our order notation is slowest -> fastest, hence TZC.
    Ok(Some(InferredLayout {
        t,
        c,
        z,
        page_order: "TZC".into(),
        source: "ImageJ/Fiji".into(),
        ifd_coords: None,
    }))
}

pub(crate) fn infer_layout_from_description(
    desc: Option<&str>,
    x: usize,
    y: usize,
    pages: usize,
    path: &Path,
) -> Result<Option<InferredLayout>, String> {
    let Some(desc) = desc else {
        return Ok(None);
    };
    if let Some(v) = infer_ome_layout(desc, x, y, pages, path)? {
        return Ok(Some(v));
    }
    if let Some(v) = infer_imagej_layout(desc, pages)? {
        return Ok(Some(v));
    }
    Ok(None)
}

/// Inferred layout when every TIFF page holds `samples` planar samples.
///
/// Logical planes are numbered page by page with the sample index varying
/// fastest; by default the samples become channels and the pages Z planes.
fn infer_multisample_layout(
    description: Option<&str>,
    physical_pages: usize,
    samples: usize,
    ds: &DatasetArgs,
) -> Result<InferredLayout, String> {
    if !ds.ignore_metadata {
        if let Some(desc) = description {
            let imagej = desc
                .lines()
                .any(|line| line.trim_start().starts_with("ImageJ="));
            if imagej || desc.contains("<OME") || desc.contains(":OME") {
                return Err(format!(
                    "TIFF pages store {samples} planar samples per pixel and also carry OME/ImageJ dimensional metadata; combining the two is not supported. Use --ignore-metadata to read the samples as channels, with --t/--c/--z/--page-order if needed"
                ));
            }
        }
    }
    Ok(InferredLayout {
        t: 1,
        c: samples,
        z: physical_pages,
        page_order: "TZC".into(),
        source: format!("planar multisample TIFF ({samples} samples per pixel read as channels)"),
        ifd_coords: None,
    })
}

/// Layout inference shared by in-memory and streaming encoding. `pages` counts
/// TIFF pages before planar samples are expanded into separate planes.
pub(super) fn infer_dataset_layout(
    description: Option<&str>,
    x: usize,
    y: usize,
    pages: usize,
    samples: usize,
    ds: &DatasetArgs,
    path: &Path,
) -> Result<Option<InferredLayout>, String> {
    if samples > 1 {
        infer_multisample_layout(description, pages, samples, ds).map(Some)
    } else if ds.ignore_metadata {
        Ok(None)
    } else {
        infer_layout_from_description(description, x, y, pages, path)
    }
}

pub(super) fn resolve_layout(
    x: usize,
    y: usize,
    pages: usize,
    ds: &DatasetArgs,
    inferred: Option<InferredLayout>,
) -> Result<ResolvedLayout, String> {
    let inferred_t = inferred.as_ref().map(|m| m.t);
    let inferred_c = inferred.as_ref().map(|m| m.c);
    let inferred_z = inferred.as_ref().map(|m| m.z);

    let t = ds.t.or(inferred_t).unwrap_or(1);
    let c = ds.c.or(inferred_c).unwrap_or(1);
    if t == 0 || c == 0 {
        return Err("T and C must be positive".into());
    }
    let tc = t.checked_mul(c).ok_or("T*C overflow")?;
    let z = match ds.z.or(inferred_z) {
        Some(z) if z > 0 => z,
        Some(_) => return Err("Z must be positive".into()),
        None => {
            if !pages.is_multiple_of(tc) {
                return Err(format!("{pages} TIFF pages are not divisible by T*C={tc}; specify --z/--t/--c explicitly"));
            }
            pages / tc
        }
    };
    let expected = tc.checked_mul(z).ok_or("T*C*Z overflow")?;
    if expected != pages {
        return Err(format!(
            "T*C*Z = {expected}, but TIFF contains {pages} pages"
        ));
    }
    let dims = Dims5 { x, y, z, c, t };

    let page_order = ds
        .page_order
        .clone()
        .or_else(|| inferred.as_ref().map(|m| m.page_order.clone()))
        .unwrap_or_else(|| "TCZ".to_string());
    validate_page_order(&page_order)?;

    let manual_shape_override = ds.t.is_some() || ds.c.is_some() || ds.z.is_some();
    let ifd_coords = if ds.page_order.is_none() && !manual_shape_override {
        inferred.as_ref().and_then(|m| m.ifd_coords.clone())
    } else {
        None
    };

    let source = match inferred {
        Some(m)
            if ds.t.is_some()
                || ds.c.is_some()
                || ds.z.is_some()
                || ds.page_order.is_some()
                || ds.ignore_metadata =>
        {
            format!("{} + CLI override", m.source)
        }
        Some(m) => m.source,
        None if ds.t.is_some()
            || ds.c.is_some()
            || ds.z.is_some()
            || ds.page_order.is_some()
            || ds.ignore_metadata =>
        {
            "CLI-specified".into()
        }
        None => "plain TIFF fallback".into(),
    };

    Ok(ResolvedLayout {
        dims,
        page_order,
        source,
        ifd_coords,
    })
}

fn axis_dim(axis: u8, d: Dims5) -> usize {
    match axis {
        b'T' => d.t,
        b'C' => d.c,
        b'Z' => d.z,
        _ => unreachable!(),
    }
}

fn axis_coord(axis: u8, t: usize, c: usize, z: usize) -> usize {
    match axis {
        b'T' => t,
        b'C' => c,
        b'Z' => z,
        _ => unreachable!(),
    }
}

/// Page order is specified slowest -> fastest. E.g. TZC means C changes
/// fastest, then Z, then T.
pub(super) fn page_index(order: &str, d: Dims5, t: usize, c: usize, z: usize) -> usize {
    let mut idx = 0usize;
    for axis in order.bytes() {
        idx = idx * axis_dim(axis, d) + axis_coord(axis, t, c, z);
    }
    idx
}

pub(super) fn reorder_pages_to_tczyx(raw: Vec<u16>, layout: &ResolvedLayout) -> Vec<u16> {
    let d = layout.dims;
    let plane = d.x * d.y;
    if let Some(ref coords) = layout.ifd_coords {
        let mut out = vec![0u16; raw.len()];
        for (src_page, &(t, c, z)) in coords.iter().enumerate() {
            let dst_page = (t * d.c + c) * d.z + z;
            let src = src_page * plane;
            let dst = dst_page * plane;
            out[dst..dst + plane].copy_from_slice(&raw[src..src + plane]);
        }
        return out;
    }
    if layout.page_order == "TCZ" {
        return raw;
    }
    let mut out = vec![0u16; raw.len()];
    for t in 0..d.t {
        for c in 0..d.c {
            for z in 0..d.z {
                let src_page = page_index(&layout.page_order, d, t, c, z);
                let dst_page = (t * d.c + c) * d.z + z;
                let src = src_page * plane;
                let dst = dst_page * plane;
                out[dst..dst + plane].copy_from_slice(&raw[src..src + plane]);
            }
        }
    }
    out
}

pub(crate) fn coords_from_page_index(
    mut page: usize,
    order: &str,
    d: Dims5,
) -> (usize, usize, usize) {
    let mut t = 0usize;
    let mut c = 0usize;
    let mut z = 0usize;
    for axis in order.bytes().rev() {
        let dim = axis_dim(axis, d);
        let coord = page % dim;
        page /= dim;
        match axis {
            b'T' => t = coord,
            b'C' => c = coord,
            b'Z' => z = coord,
            _ => unreachable!(),
        }
    }
    (t, c, z)
}

fn ome_dimension_order(page_order: &str) -> String {
    // Radelta page_order is slowest -> fastest (e.g. TCZ means Z varies fastest).
    // OME DimensionOrder is X,Y followed by fastest -> slowest non-spatial axes.
    let rev: String = page_order.chars().rev().collect();
    format!("XY{rev}")
}

pub(crate) fn ome_description(d: Dims5, order: &str) -> String {
    let dim_order = ome_dimension_order(order);
    let mut channels = String::new();
    for c in 0..d.c {
        channels.push_str(&format!(
            r#"<Channel ID="Channel:0:{}" SamplesPerPixel="1"/>"#,
            c
        ));
    }
    let plane_count = d.t.saturating_mul(d.c).saturating_mul(d.z);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06"><Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="{}" Type="uint16" SizeX="{}" SizeY="{}" SizeZ="{}" SizeC="{}" SizeT="{}">{}<TiffData IFD="0" PlaneCount="{}"/></Pixels></Image></OME>"#,
        dim_order, d.x, d.y, d.z, d.c, d.t, channels, plane_count
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_imagej_hyperstack() {
        let d = "ImageJ=1.54f\nimages=24\nchannels=2\nslices=3\nframes=4\nhyperstack=true\n";
        let m = infer_imagej_layout(d, 24).unwrap().unwrap();
        assert_eq!((m.t, m.c, m.z), (4, 2, 3));
        assert_eq!(m.page_order, "TZC");
    }

    #[test]
    fn parses_ome_dimension_order() {
        let d = r#"<?xml version="1.0"?><OME><Image><Pixels DimensionOrder="XYZTC" SizeX="10" SizeY="7" SizeZ="3" SizeT="2" SizeC="2"><TiffData/></Pixels></Image></OME>"#;
        let m = infer_ome_layout(d, 10, 7, 12, Path::new("image.ome.tif"))
            .unwrap()
            .unwrap();
        assert_eq!((m.t, m.c, m.z), (2, 2, 3));
        assert_eq!(m.page_order, "CTZ");
        assert!(m.ifd_coords.is_none());
    }

    #[test]
    fn parses_ome_explicit_reverse_time_mapping() {
        let d = r#"<OME><Image><Pixels DimensionOrder="XYZTC" SizeX="10" SizeY="7" SizeZ="1" SizeC="1" SizeT="3"><TiffData IFD="0" FirstT="2"/><TiffData IFD="1" FirstT="1"/><TiffData IFD="2" FirstT="0"/></Pixels></Image></OME>"#;
        let m = infer_ome_layout(d, 10, 7, 3, Path::new("image.ome.tif"))
            .unwrap()
            .unwrap();
        let map = m.ifd_coords.unwrap();
        assert_eq!(map, vec![(2, 0, 0), (1, 0, 0), (0, 0, 0)]);
    }

    #[test]
    fn page_order_validation_accepts_all_permutations() {
        for order in ["TCZ", "TZC", "CTZ", "CZT", "ZTC", "ZCT"] {
            assert!(validate_page_order(order).is_ok(), "{order}");
        }
        for order in ["", "TC", "TTZ", "XYZ", "TCZZ"] {
            assert!(validate_page_order(order).is_err(), "{order}");
        }
    }

    #[test]
    fn page_index_and_inverse_are_consistent() {
        let d = Dims5 {
            x: 1,
            y: 1,
            z: 3,
            c: 2,
            t: 4,
        };
        for order in ["TCZ", "TZC", "CTZ", "CZT", "ZTC", "ZCT"] {
            for t in 0..d.t {
                for c in 0..d.c {
                    for z in 0..d.z {
                        let page = page_index(order, d, t, c, z);
                        assert_eq!(coords_from_page_index(page, order, d), (t, c, z), "{order}");
                    }
                }
            }
        }
    }

    #[test]
    fn imagej_metadata_page_count_must_match() {
        let d = "ImageJ=1.54f\nimages=24\nchannels=2\nslices=3\nframes=4\nhyperstack=true\n";
        assert!(infer_imagej_layout(d, 23).is_err());
    }
}
